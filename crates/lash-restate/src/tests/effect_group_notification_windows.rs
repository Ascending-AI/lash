//! ADR 0099's crash windows where the group index answers its own notices
//! (FIG-4344), at the authority boundaries the notices moved to.
//!
//! A notice's authority is the index handler whose state change makes it
//! true: that handler stores its decision, then the subscribers it keeps,
//! then completes the rest from its own journal. The windows below crash each
//! such handler, and each waiter, at those boundaries, and require the ADR's
//! outcome with the notices still owned by the index:
//!
//! | window | cut |
//! |---|---|
//! | W1 | a subscription stored, its answer lost (`subscribe` before its output) |
//! | W2, W3 | the registration stored, its READY subscribers not yet completed |
//! | W4 | a seat stored, its RANK, barrier and cancel-fact subscribers not yet completed |
//! | W6, W19 | a final committed and its rank reserved, the commit's answer, the presentation or the payload lost before the seat |
//! | W14 | a child handler dies at its first call, before its cancel read, with the opener alive |
//! | W9 | the close stored its cancel decisions, before its subscribers are completed or any child is cancelled |
//! | W17 | the close fences a decided child's completion before it stores, notifies or cancels |
//!
//! Each window runs on the server double as recorded and with every await
//! suspended and replayed, over SQLite memory and SQLite file stores, and on
//! PostgreSQL where the gate provides it. The windows the notices did not
//! move (W5, W7, W8, W10–W13, W15, W16, W18, W20) keep their laws where they
//! are; `docs/adr/0099-…` names them.

use std::sync::Arc;

use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestServer};

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use super::effect_group_notification_ownership::{
    assert_children_issued_before_any_wait, assert_no_generic_group_waits,
    assert_seats_invoke_nothing,
};
use super::effect_group_seat_chain::{
    JournalKeepingRunner, assert_admission_chain, assert_seat_chains, dispatch_services,
    group_invocations, run_batch_over,
};

/// One crash window of the width-4 batch: its ADR 0099 windows, the rules
/// that crash the first attempt at its cut, and the handlers the crash must
/// strike.
struct Window {
    name: &'static str,
    rules: fn(&RestateTestServer) -> Vec<CrashRule>,
    struck: &'static [&'static str],
}

fn index_cut(handler: &str, types: &[MessageType]) -> Vec<CrashRule> {
    types
        .iter()
        .map(|&ty| {
            CrashRule::new(CrashPoint::BeforeFrame { ty })
                .service("EffectGroupIndex")
                .handler(handler)
                .within_attempts(1)
        })
        .collect()
}

/// Between a handler's stored decision and its answer: before its first
/// completion of a subscriber, or, with none to complete, before its output.
const BEFORE_NOTIFYING: [MessageType; 2] = [
    MessageType::CompleteAwakeableCommand,
    MessageType::OutputCommand,
];

const BATCH_WINDOWS: [Window; 5] = [
    Window {
        name: "W1: a subscription stored, its answer lost",
        rules: |_| index_cut("subscribe", &[MessageType::OutputCommand]),
        struck: &["/subscribe"],
    },
    Window {
        name: "W2/W3: the registration stored, its READY subscribers not completed",
        rules: |_| index_cut("register_dispatch", &BEFORE_NOTIFYING),
        struck: &["/register_dispatch"],
    },
    Window {
        name: "W4: a seat stored, its subscribers not completed",
        rules: |_| index_cut("record_settlement", &BEFORE_NOTIFYING),
        struck: &["/record_settlement"],
    },
    Window {
        name: "W6/W19: a final committed and its rank reserved, lost before the seat",
        rules: |server| {
            let mut rules = index_cut("commit_child", &[MessageType::OutputCommand]);
            for lane in dispatch_services(server) {
                rules.push(
                    CrashRule::new(CrashPoint::BeforeRunResultEnding {
                        suffix: ":present".to_string(),
                    })
                    .service(lane)
                    .handler("child")
                    .within_attempts(1),
                );
            }
            rules.push(
                CrashRule::new(CrashPoint::BeforeFrame {
                    ty: MessageType::OutputCommand,
                })
                .service("EffectGroupPayload")
                .handler("put")
                .within_attempts(1),
            );
            rules
        },
        struck: &["/commit_child", "/child", "/put"],
    },
    Window {
        name: "W14: a child handler dies at its first call, the opener alive",
        rules: |server| {
            dispatch_services(server)
                .into_iter()
                .map(|lane| {
                    CrashRule::new(CrashPoint::BeforeFrame {
                        ty: MessageType::CallCommand,
                    })
                    .service(lane)
                    .handler("child")
                    .within_attempts(1)
                })
                .collect()
        },
        struck: &["/child"],
    },
];

/// The stores a window runs over.
#[derive(Clone, Copy, Debug)]
enum Storage {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

impl Storage {
    fn label(self) -> &'static str {
        match self {
            Self::SqliteMemory => "sqlite-memory",
            Self::SqliteFile => "sqlite-file",
            Self::Postgres => "postgres",
        }
    }

    async fn open(self, directory: &std::path::Path) -> Arc<dyn lash_core::StoreSet> {
        match self {
            Self::SqliteMemory => Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("SQLite memory stores"),
            ),
            Self::SqliteFile => Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(directory.join("sqlite"))
                    .await
                    .expect("SQLite file stores"),
            ),
            Self::Postgres => Arc::new(lash_postgres_store::PostgresStoreSet::new(
                &super::batch_oracle::postgres_fixture().await,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    directory.join("attachments"),
                )),
            )),
        }
    }
}

/// An endpoint on the server double for notification replay.
async fn double(
    always_replay: bool,
    _stores: &Arc<dyn lash_core::StoreSet>,
) -> LiveConformanceHarness {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await
}

/// Every batch window, crashed once at its cut: the batch answers every
/// member, each child commits once and seats its reserved rank once, the
/// dispatch registers once, and no invocation reaches the generic
/// durable-wait services for a group notice.
async fn batch_windows_hold(storage: Storage) {
    for always_replay in [false, true] {
        for window in &BATCH_WINDOWS {
            let directory = tempfile::tempdir().expect("store fixture directory");
            let stores = storage.open(directory.path()).await;
            let harness = double(always_replay, &stores).await;
            let server = harness
                .server_double()
                .expect("the windows crash handlers on the server double");
            for rule in (window.rules)(&server) {
                server.crash_on(rule);
            }
            let label = format!(
                "notice-window-{}-{}-{}",
                storage.label(),
                harness.run_nonce(),
                always_replay
            );
            run_batch_over(&harness, &label, stores).await;
            let (dispatch, children) = group_invocations(&server).await;
            let crashed = server
                .invocations()
                .into_iter()
                .filter(|view| view.attempts > 1)
                .map(|view| view.target)
                .collect::<Vec<_>>();
            for handler in window.struck {
                assert!(
                    crashed.iter().any(|target| target.ends_with(handler)),
                    "{} ({}, replay {always_replay}): a crash struck {handler}: {crashed:?}",
                    window.name,
                    storage.label()
                );
            }
            assert_seat_chains(&server, &children);
            assert_admission_chain(&server, &dispatch);
            assert_seats_invoke_nothing(&server);
            assert_no_generic_group_waits(&server, &children);
            assert_children_issued_before_any_wait(&server, &dispatch);
            println!(
                "FIG-4344 window PASS {} ({}, replay {always_replay})",
                window.name,
                storage.label()
            );
            harness.finish().await;
        }
    }
}

/// The close of a cancelled batch, read from its journal: every durable-wait
/// write that seals a cancel decision — the fence of a decided tool child's
/// completion, the release of a decided wait — precedes the stored decision;
/// the stored decision precedes every completion of a subscriber; and every
/// completion precedes the first cancel of a child's invocation.
fn assert_close_fences_before_it_notifies_and_cancels(server: &RestateTestServer) {
    let closes = super::effect_group_notification_ownership::index_invocations(server, "close")
        .into_iter()
        .filter(|view| {
            server.journal(&view.id).is_some_and(|journal| {
                journal
                    .iter()
                    .any(|entry| entry.ty == MessageType::SendSignalCommand)
            })
        })
        .collect::<Vec<_>>();
    assert!(
        !closes.is_empty(),
        "the cancelled batch's close cancels its undecided children"
    );
    for close in closes {
        let journal = server.journal(&close.id).expect("the close's journal");
        let journaled = journal
            .iter()
            .map(|entry| format!("{:?}", entry.ty))
            .collect::<Vec<_>>();
        let decision = journal
            .iter()
            .position(|entry| entry.written_state_key().as_deref() == Some("effect-group/v1/state"))
            .unwrap_or_else(|| panic!("the close stores its decision: {journaled:?}"));
        let positions = |ty: MessageType| {
            journal
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.ty == ty)
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        let seals = positions(MessageType::CallCommand);
        let completions = positions(MessageType::CompleteAwakeableCommand);
        let cancels = positions(MessageType::SendSignalCommand);
        assert!(
            seals.iter().all(|&index| index < decision),
            "every seal of a cancel decision precedes the stored decision: {journaled:?}"
        );
        assert!(
            !completions.is_empty(),
            "the close completes its decided children's cancel watches: {journaled:?}"
        );
        assert!(
            completions.iter().all(|&index| decision < index),
            "the decision is stored before any subscriber hears of it: {journaled:?}"
        );
        let first_cancel = cancels.iter().min().expect("a cancel");
        assert!(
            completions.iter().all(|index| index < first_cancel),
            "every subscriber is completed before any child is cancelled: {journaled:?}"
        );
        assert!(
            journal
                .iter()
                .filter_map(|entry| entry.call_command())
                .all(|call| call.service_name.starts_with("LashDurableWaitIndex")),
            "the close calls only the durable-wait index, to seal decisions: {journaled:?}"
        );
    }
}

/// W9 and W17 on the cancelled batch: the close crashed after it stored its
/// decisions and before it completed a subscriber, and again before it
/// cancelled a child; the redriven close keeps its fence order, the batch's
/// committed row stands and its undecided rows say cancelled.
async fn close_windows_hold(storage: Storage, always_replay_modes: &[bool]) {
    for &always_replay in always_replay_modes {
        for (name, ty) in [
            (
                "W9: before the close's first completion",
                MessageType::CompleteAwakeableCommand,
            ),
            (
                "W9: before the close's first cancel",
                MessageType::SendSignalCommand,
            ),
        ] {
            let directory = tempfile::tempdir().expect("store fixture directory");
            let stores = storage.open(directory.path()).await;
            let harness = double(always_replay, &stores).await;
            let server = harness
                .server_double()
                .expect("the windows crash handlers on the server double");
            for rule in index_cut("close", &[ty]) {
                server.crash_on(rule);
            }
            let prefix = format!(
                "notice-close-{}-{}-{always_replay}",
                storage.label(),
                harness.run_nonce()
            );
            lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
                &prefix,
                harness.endpoint_host(),
                stores,
                Arc::new(JournalKeepingRunner(harness.turn_runner())),
                super::batch_oracle::factories(),
            )
            .await;
            assert!(
                super::effect_group_notification_ownership::index_invocations(&server, "close")
                    .iter()
                    .any(|view| view.attempts > 1),
                "{name} ({}, replay {always_replay}): the crash struck the close",
                storage.label()
            );
            assert_close_fences_before_it_notifies_and_cancels(&server);
            println!(
                "FIG-4344 window PASS {name} ({}, replay {always_replay})",
                storage.label()
            );
            harness.finish().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn notification_windows_hold_on_sqlite_memory() {
    batch_windows_hold(Storage::SqliteMemory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn notification_windows_hold_on_sqlite_file() {
    batch_windows_hold(Storage::SqliteFile).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires isolated PostgreSQL; run under scripts/ci/with-service.sh pg16"]
async fn notification_windows_hold_on_postgres() {
    batch_windows_hold(Storage::Postgres).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn close_windows_hold_on_sqlite_memory() {
    close_windows_hold(Storage::SqliteMemory, &[false, true]).await;
}

/// The cancelled batch under forced replay over a SQLite file is held back by
/// FIG-4364 (the assembled batch row contradicts the durable final), so this
/// store runs it as recorded only.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn close_windows_hold_on_sqlite_file() {
    close_windows_hold(Storage::SqliteFile, &[false]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires isolated PostgreSQL; run under scripts/ci/with-service.sh pg16"]
async fn close_windows_hold_on_postgres() {
    close_windows_hold(Storage::Postgres, &[false]).await;
}

/// The width-4 batch and the cancelled batch on live Restate over every
/// store: SQLite memory, SQLite file and PostgreSQL. A live server keeps no
/// journal the laws could read, so here the batch's answers and the cancelled
/// batch's own laws are the evidence; the windows above read the journals.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires isolated Restate and PostgreSQL; run through the effect-group suite with pg16"]
async fn live_notification_batches_hold_over_every_store() {
    for storage in [
        Storage::SqliteMemory,
        Storage::SqliteFile,
        Storage::Postgres,
    ] {
        let directory = tempfile::tempdir().expect("store fixture directory");
        let stores = storage.open(directory.path()).await;
        // Each store set is its law's ledger, so each gets its own endpoint.
        let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::Live).await;
        let label = format!(
            "live-notice-batch-{}-{}",
            storage.label(),
            harness.run_nonce()
        );
        run_batch_over(&harness, &label, Arc::clone(&stores)).await;
        let prefix = format!(
            "live-notice-close-{}-{}",
            storage.label(),
            harness.run_nonce()
        );
        lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
            &prefix,
            harness.endpoint_host(),
            stores,
            harness.turn_runner(),
            super::batch_oracle::factories(),
        )
        .await;
        println!("FIG-4344 live batches PASS ({})", storage.label());
        harness.finish().await;
    }
}
