//! FIG-5135: an open source takes part in the Run's recorded selection, so
//! a resolved Deferred call is decided while a sibling body still runs, and
//! a crash on either side of the schedule record that selected its seal
//! replays to one decision.
//!
//! B parks on its source; A's body holds until B's decision is durable. A
//! schedule that offered only running bodies would wait on A forever.

use super::*;

use lash_core::tool_run::{
    MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole, SealWriter,
    SourceSeal,
};

/// A fresh double whose Run reads its source results from SQLite stores,
/// in a file under `dir` or in memory.
async fn backend(
    seed: u64,
    dir: Option<&tempfile::TempDir>,
) -> (
    RestateTestBackend<dyn lash_core::StoreSet>,
    Arc<dyn lash_core::store::ToolMaterialStore>,
) {
    let stores = Arc::new(match dir {
        Some(dir) => lash_sqlite_store::SqliteStoreSet::open(dir.path())
            .await
            .unwrap(),
        None => lash_sqlite_store::SqliteStoreSet::memory().await.unwrap(),
    });
    let materials = stores.process_env_store() as Arc<dyn lash_core::store::ToolMaterialStore>;
    let backend = lash_restate_test::backend_with_store_set(
        seed,
        ServerConfig::default(),
        lash_restate_test::DeploymentHooks::default(),
        move |_clock| async move { Ok(stores as Arc<dyn lash_core::StoreSet>) },
    )
    .await
    .unwrap();
    (backend, materials)
}

/// The handler's journal entries, every attempt's in order.
fn handler_journal(
    server: &lash_restate_test::RestateTestServer,
) -> Vec<lash_restate_test::JournalEntryView> {
    server
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with("LashTestHandlerHost/"))
        .flat_map(|view| server.journal(&view.id).unwrap())
        .collect()
}

/// Whether a durable Run record of the handler holds an event `matches`
/// accepts.
fn durable_event(
    server: &lash_restate_test::RestateTestServer,
    matches: impl Fn(&RunEvent) -> bool,
) -> bool {
    handler_journal(server).iter().any(|entry| {
        let Some(Ok(bytes)) = entry.run_completion() else {
            return false;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return false;
        };
        let Some(record) = value.get("record") else {
            return false;
        };
        serde_json::from_value::<RunRecord>(record.clone())
            .is_ok_and(|record| record.events.iter().any(&matches))
    })
}

/// Seal `call_id`'s source with its retained done result, as its external
/// completer would.
async fn seal_resolved(
    backend: &RestateTestBackend<dyn lash_core::StoreSet>,
    probe: &Probe,
    call_id: &ToolCallId,
) {
    let source = probe.sources.lock().unwrap()[call_id].clone();
    let capture = SingletonCapture::Done {
        output: output_of(call_id),
        commands: Vec::new(),
        intents: Vec::new(),
        stream: Default::default(),
        start: None,
    };
    let bundle = MaterialBundle::of([MaterialPayload::new(
        MaterialOwner::Source {
            source: source.clone(),
        },
        MaterialRole::AttemptOutput,
        Some(revision()),
        serde_json::to_string(&capture).unwrap(),
    )])
    .unwrap()
    .unwrap();
    let retained = probe
        .materials
        .as_ref()
        .unwrap()
        .retain_material(
            &MaterialHolder::Source {
                source: source.clone(),
            },
            &bundle,
        )
        .await
        .unwrap();
    let reply: crate::Reply<crate::durable_wait::RestateSourceSealReply> = backend
        .ingress()
        .call_object_json(
            "LashDurableWaitIndex",
            "session",
            "seal_source",
            &crate::Call::new(crate::durable_wait::RestateSourceSealRequest {
                source,
                writer: SealWriter::External,
                seal: SourceSeal::Resolved {
                    result: Box::new(retained.references[0].clone()),
                },
            }),
        )
        .await
        .unwrap();
    assert!(matches!(
        reply.into_body(),
        crate::durable_wait::RestateSourceSealReply::Outcome { .. }
    ));
}

struct Selected {
    journal: Vec<String>,
    records: Vec<RunRecord>,
    probe: Arc<Probe>,
}

/// One Run of held A and Deferred B. A's body is released only once B's
/// final decision is durable, so B can be decided only by a schedule that
/// selects its seal beside A's running body.
async fn source_selected_beside_a_held_body(
    dir: Option<&tempfile::TempDir>,
    crash: Option<CrashPoint>,
) -> Selected {
    let (backend, materials) = backend(0x5135, dir).await;
    let held = call("held", &Kind::IntentFree);
    let parked = call("parked", &Kind::Deferred);
    let round = vec![held.clone(), parked.clone()];
    let gate = Arc::new(Gate::default());
    let mut probe = Probe::new(&[
        (held.clone(), Kind::IntentFree),
        (parked.clone(), Kind::Deferred),
    ]);
    probe.materials = Some(materials);
    probe.gates.insert(held.call_id.clone(), Arc::clone(&gate));
    let probe = Arc::new(probe);
    if let Some(point) = crash {
        backend.server().crash_on(CrashRule::new(point));
    }
    let records = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let probe = Arc::clone(&probe);
        let records = Arc::clone(&records);
        Arc::new(move |scoped| {
            let probe = Arc::clone(&probe);
            let records = Arc::clone(&records);
            let round = round.clone();
            Box::pin(async move {
                probe.handler_attempts.fetch_add(1, Ordering::SeqCst);
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                super::super::decide_round(
                    &mut run,
                    &round,
                    Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                    Default::default(),
                )
                .await
                .unwrap();
                run.await_deferred().await.unwrap();
                run.drain().await.unwrap();
                *records.lock().unwrap() = run.into_records();
            })
        })
    };
    let script = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !handler_journal(backend.server()).iter().any(|entry| {
            entry
                .call_command()
                .is_some_and(|call| call.handler_name == "subscribe_source")
        }) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the schedule never subscribed B's source while A's body ran"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        seal_resolved(&backend, &probe, &parked.call_id).await;
        while !durable_event(backend.server(), |event| {
            matches!(event, RunEvent::Decided {
                call_id,
                decision: CallDecision::Final { .. },
                ..
            } if *call_id == parked.call_id)
        }) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "B's resolved source was never decided while A's body ran"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        gate.release();
    };
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            async {
                backend
                    .run_in_handler(AdmittedScope::turn("session", "turn"), attempt)
                    .await
                    .unwrap()
            },
            script,
        )
    })
    .await
    .unwrap();
    backend.server().settle().await;
    let journal = handler_journal(backend.server())
        .into_iter()
        .filter(|entry| entry.ty == MessageType::RunCommand)
        .filter_map(|entry| entry.name)
        .collect();
    let records = records.lock().unwrap().clone();
    Selected {
        journal,
        records,
        probe,
    }
}

fn selected_seals(records: &[RunRecord], call_id: &ToolCallId) -> usize {
    records
        .iter()
        .flat_map(|record| &record.events)
        .filter(
            |event| matches!(event, RunEvent::SourceSealed { call_id: id, .. } if id == call_id),
        )
        .count()
}

fn decisions(records: &[RunRecord], call_id: &ToolCallId) -> Vec<CallDecision> {
    records
        .iter()
        .flat_map(|record| &record.events)
        .filter_map(|event| match event {
            RunEvent::Decided {
                call_id: id,
                decision,
                ..
            } if id == call_id => Some(decision.clone()),
            _ => None,
        })
        .collect()
}

/// The seal is the schedule's recorded selection, taken while A's body ran,
/// and decides B once, ahead of A.
fn assert_selected_once(selected: &Selected, label: &str) {
    let held = ToolCallId::fixture("held");
    let parked = ToolCallId::fixture("parked");
    assert_eq!(
        selected_seals(&selected.records, &parked),
        1,
        "{label}: one recorded selection names B's seal"
    );
    let decided = decisions(&selected.records, &parked);
    assert!(
        matches!(decided.as_slice(), [CallDecision::Final { .. }]),
        "{label}: B is decided once, from its seal: {decided:?}"
    );
    assert!(
        matches!(
            decisions(&selected.records, &held).as_slice(),
            [CallDecision::Final { .. }]
        ),
        "{label}: A is decided once"
    );
    let rank = |call_id: &ToolCallId| {
        selected
            .records
            .iter()
            .flat_map(|record| {
                record
                    .events
                    .iter()
                    .enumerate()
                    .map(move |(index, event)| (record.first.0 + index as u64, event))
            })
            .find_map(|(ordinal, event)| {
                matches!(event, RunEvent::Decided { call_id: id, .. } if id == call_id)
                    .then_some(ordinal)
            })
            .unwrap()
    };
    assert!(
        rank(&parked) < rank(&held),
        "{label}: B's seal decides it while A's body still runs"
    );
    assert_eq!(
        selected.probe.executions_of(&parked),
        1,
        "{label}: B's Deferred attempt is served on replay"
    );
    assert_eq!(
        selected
            .probe
            .presentations
            .lock()
            .unwrap()
            .iter()
            .filter(|id| **id == parked)
            .count(),
        1,
        "{label}: B is presented once"
    );
}

async fn a_crash_on_either_side_of_a_selected_seal_replays_one_decision(
    dir: impl Fn() -> Option<tempfile::TempDir>,
) {
    let whole_dir = dir();
    let whole = source_selected_beside_a_held_body(whole_dir.as_ref(), None).await;
    assert_selected_once(&whole, "whole");
    let selection = whole
        .records
        .iter()
        .find(|record| {
            record
                .events
                .iter()
                .any(|event| matches!(event, RunEvent::SourceSealed { .. }))
        })
        .map(|record| schedule(record.first.0))
        .expect("a schedule record selected B's seal");
    assert!(
        whole.journal.contains(&selection),
        "the selected seal is the schedule's own record"
    );
    for (label, point) in [
        (
            "crash before the seal is accepted",
            CrashPoint::BeforeRunResult {
                name: Some(selection.clone()),
            },
        ),
        (
            "crash after the seal is accepted",
            CrashPoint::AfterRunResult {
                name: selection.clone(),
            },
        ),
    ] {
        let crashed_dir = dir();
        let crashed = source_selected_beside_a_held_body(crashed_dir.as_ref(), Some(point)).await;
        assert!(
            crashed.probe.handler_attempts.load(Ordering::SeqCst) > 1,
            "{label}: the crash replayed the handler"
        );
        assert_selected_once(&crashed, label);
        assert_eq!(
            crashed.journal, whole.journal,
            "{label}: replay issues the whole run's records"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_a_crash_on_either_side_of_a_selected_seal_replays_one_decision() {
    a_crash_on_either_side_of_a_selected_seal_replays_one_decision(|| None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_file_a_crash_on_either_side_of_a_selected_seal_replays_one_decision() {
    a_crash_on_either_side_of_a_selected_seal_replays_one_decision(|| {
        Some(tempfile::tempdir().unwrap())
    })
    .await;
}
