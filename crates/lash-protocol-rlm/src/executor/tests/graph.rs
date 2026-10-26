//! The laws of a session's paged state (FIG-6193): saved functions run
//! with what they froze after a later rebinding and a cold restore, a
//! working state is adopted whole and a rolled-back cell never promotes
//! what it captured, states restored from one descriptor share its pages
//! but not their heaps, and a host read of more pages than the page cache
//! holds completes.

use std::sync::Arc;

use lash_core_store::execution_state::{ExecutionGraphCapture, ExecutionGraphPurpose};

use super::saved_functions::NoTools;
use super::{
    CodeModeExecutionState, SESSION, TURN, finish_of, open_host, typescript_services,
    typescript_state,
};
use crate::testing::{DurableHost, cell_context, commit_working, run_cell};

async fn cell(
    state: &mut CodeModeExecutionState,
    host: &DurableHost,
    key: &'static str,
    code: &str,
) -> lash_core::ExecResponse {
    let response = run_cell(
        state,
        cell_context(host, SESSION, TURN, key, Arc::new(NoTools)),
        &typescript_services(),
        code,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    response
}

/// A fresh session restored from `state`'s descriptor, which `host`
/// committed as its turn's working state.
async fn restored(host: &DurableHost, state: &CodeModeExecutionState) -> CodeModeExecutionState {
    let graph = state
        .hydrated_execution_graph()
        .await
        .expect("the session holds a descriptor");
    let mut restored = typescript_state();
    restored
        .restore_execution_graph(&graph, ExecutionGraphPurpose::Working)
        .expect("the descriptor restores");
    let _ = host;
    restored
}

/// The root a working commit names and how many pages it published.
fn sent(
    (capture, published): &(ExecutionGraphCapture, u64),
) -> (lash_core_store::checkpoint_graph::NodeId, u64) {
    (root_of(capture), *published)
}

/// The root a capture names.
fn root_of(capture: &ExecutionGraphCapture) -> lash_core_store::checkpoint_graph::NodeId {
    let ExecutionGraphCapture::Replace(graph) = capture else {
        panic!("a session with bindings captures a root");
    };
    graph.root
}

/// Frozen captures and session carry: a function saved in one cell keeps
/// what it read when a later cell rebinds it, mutually recursive saved
/// functions call one another, and an alias holds the function it was
/// bound to. Restored cold from the committed descriptor alone, in a
/// session that never ran the cells, every function runs with what it
/// froze. The rebinding's commit sends fewer pages than the cell that
/// made the functions.
#[tokio::test(flavor = "multi_thread")]
async fn saved_functions_keep_what_they_froze_across_a_rebinding_and_a_cold_restore() {
    let host = open_host().await;
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        "let rate = 3;\nfunction isEven(n: number): boolean { return n === 0 ? true : isOdd(n - 1); }\nfunction isOdd(n: number): boolean { return n === 0 ? false : isEven(n - 1); }\nconst scale = (x: number) => x * rate;\nconst alias = scale;",
    )
    .await;
    let (first, made) = sent(&commit_working(&mut state, &host).await);

    cell(&mut state, &host, "exec-code:1", "rate = 100;").await;
    let (second, rebound) = sent(&commit_working(&mut state, &host).await);
    assert_ne!(second, first, "the rebinding is the session's");
    assert!(
        rebound < made,
        "the rebinding sends only the pages it changed: {rebound} of {made}"
    );
    let mut cold = restored(&host, &state).await;
    drop(state);
    let called = cell(
        &mut cold,
        &host,
        "exec-code:2",
        "await control.finish([isEven(4), isOdd(4), scale(2), alias(2), rate]);",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!([true, false, 6, 6, 100])
    );
}

/// Fork isolation: two sessions restored from one descriptor read its
/// pages, but each has a heap of its own: what one cell changes, the other
/// never sees.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_restored_from_one_descriptor_share_its_pages_but_not_their_heaps() {
    let host = open_host().await;
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        &format!(
            "const notes = [\"{}\"];\nfunction label(x: string) {{ return 'n:' + x; }}",
            "x".repeat(1_000)
        ),
    )
    .await;
    commit_working(&mut source, &host).await;
    let mut left = restored(&host, &source).await;
    let mut right = restored(&host, &source).await;
    drop(source);
    cell(
        &mut left,
        &host,
        "exec-code:1",
        "notes.push(label('more'));",
    )
    .await;
    let seen = cell(
        &mut right,
        &host,
        "exec-code:2",
        "await control.finish(notes.length);",
    )
    .await;
    assert_eq!(
        finish_of(&seen),
        serde_json::json!(1),
        "the right heap is its own"
    );
    let grown = cell(
        &mut left,
        &host,
        "exec-code:3",
        "await control.finish(notes[1]);",
    )
    .await;
    assert_eq!(finish_of(&grown), serde_json::json!("n:more"));
}

/// Lease time advances only at the law's named boundary, independently of
/// how long workers or the shared test host take to run a cell; so does
/// the wall time a cell saves for holding them.
#[derive(Debug)]
pub(super) struct LeaseClock {
    epoch: std::time::Instant,
    elapsed: tokio::sync::watch::Sender<std::time::Duration>,
}

impl LeaseClock {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            // A distinct origin also catches accidental OS-clock elapsed reads.
            epoch: std::time::Instant::now() - super::super::pages::LEASE_TTL,
            elapsed: tokio::sync::watch::channel(std::time::Duration::ZERO).0,
        })
    }

    fn advance_to(&self, elapsed: std::time::Duration) {
        self.elapsed.send_replace(elapsed);
    }
}

#[async_trait::async_trait]
impl lash_core::Clock for LeaseClock {
    fn now(&self) -> std::time::Instant {
        self.epoch + *self.elapsed.borrow()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000) + *self.elapsed.borrow(),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        self.sleep_until(self.now() + duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.elapsed
            .subscribe()
            .wait_for(|elapsed| self.epoch + *elapsed >= deadline)
            .await
            .expect("the law owns its clock");
    }
}

async fn lease_host(clock: Arc<LeaseClock>) -> DurableHost {
    DurableHost::open_with_clock(lash_core::AdmittedScope::turn(SESSION, TURN), clock).await
}

/// Session page authority (FIG-6193): the cells of a turn read the
/// session's committed pages under one lease, which the first read takes
/// and every later cell reuses before renewal is due, so N cells make
/// one lease write; and a
/// cell that rebinds one binding of many commits only the pages it
/// changed, never the session's whole heap.
#[tokio::test(flavor = "multi_thread")]
async fn a_turns_cells_share_one_lease_and_commit_only_the_pages_they_changed() {
    let clock = LeaseClock::new();
    let host = lease_host(clock.clone()).await;
    let mut state = typescript_state();
    state.page_clock(clock);
    let mut patch = lash_rlm_types::RlmGlobalsPatchPluginBody::default();
    for index in 0..256 {
        patch.set_default.insert(
            format!("b{index}"),
            serde_json::json!(format!("{index}:{}", "x".repeat(1024))),
        );
    }
    state
        .patch_globals(&patch)
        .await
        .expect("bind the session's values");
    let (_, whole) = sent(&commit_working(&mut state, &host).await);
    for (key, code) in [
        ("exec-code:0", "const r0 = b0.length;"),
        ("exec-code:1", "const r1 = b1.length;"),
        ("exec-code:2", "const r2 = b2.length;"),
        ("exec-code:3", "b7 = 'changed';"),
    ] {
        cell(&mut state, &host, key, code).await;
    }
    assert_eq!(
        state.lease_writes(),
        1,
        "the turn's cells read under the one lease the first read took"
    );
    let (_, changed) = sent(&commit_working(&mut state, &host).await);
    assert!(
        changed * 8 < whole,
        "the turn commits the pages its cells changed: {changed} of the session's {whole}"
    );
}

/// D-LEASE / FIG-7135: the host clock schedules one renewal at a third
/// of the TTL, preserving the existing lease identity and extending its expiry.
#[tokio::test(flavor = "multi_thread")]
async fn a_turns_page_lease_renews_on_its_host_clock_without_changing_identity() {
    use super::super::pages::{LEASE_TTL, SessionPageAccess};

    let clock = LeaseClock::new();
    let host = lease_host(clock.clone()).await;
    let mut state = typescript_state();
    state.page_clock(clock.clone());
    state
        .patch_globals(&lash_rlm_types::RlmGlobalsPatchPluginBody {
            set_default: serde_json::Map::from_iter([("kept".to_owned(), serde_json::json!(1))]),
        })
        .await
        .unwrap();
    commit_working(&mut state, &host).await;
    let handle = host.session_pages().unwrap();
    let mut access = SessionPageAccess::default();
    access.clock(clock.clone());
    access.note(Some(&handle));
    let first = access.lease().await.unwrap().unwrap();
    let before_renewal = LEASE_TTL / 3 - std::time::Duration::from_millis(1);
    clock.advance_to(before_renewal);
    assert_eq!(access.lease().await.unwrap().unwrap(), first);
    assert_eq!(access.lease_writes(), 1, "no renewal before it is due");

    clock.advance_to(LEASE_TTL / 3);
    let renewed = access.lease().await.unwrap().unwrap();
    assert_eq!(renewed.id, first.id, "renew the turn's existing lease");
    assert_eq!(renewed.root, first.root);
    assert!(renewed.expires_at > first.expires_at);
    assert_eq!(access.lease_writes(), 2, "one acquisition and one renewal");
    assert_eq!(access.lease().await.unwrap().unwrap(), renewed);
    assert_eq!(access.lease_writes(), 2, "reuse the confirmed renewal");
    handle
        .store
        .read_pages(&renewed, &[handle.root])
        .await
        .unwrap();
    access.release().await;
}

/// The session descriptor is bounded (FIG-6418 L1): a session that binds
/// ten thousand names, one of them 200 KiB, still seals its descriptor,
/// restores cold from it and reads a binding in a cell.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_with_ten_thousand_bindings_and_a_200_kib_name_seals_reopens_and_runs_a_cell() {
    let host = open_host().await;
    let mut state = typescript_state();
    let mut patch = lash_rlm_types::RlmGlobalsPatchPluginBody::default();
    for index in 0..10_000 {
        patch
            .set_default
            .insert(format!("b{index}"), serde_json::json!(index));
    }
    patch
        .set_default
        .insert("n".repeat(200 * 1024), serde_json::json!("wide"));
    state
        .patch_globals(&patch)
        .await
        .expect("the host's defaults apply");
    commit_working(&mut state, &host).await;
    let mut cold = restored(&host, &state).await;
    drop(state);
    let read = cell(
        &mut cold,
        &host,
        "exec-code:0",
        "await control.finish(b42);",
    )
    .await;
    assert_eq!(finish_of(&read), serde_json::json!(42));
}

/// A no-op cell writes the same pages and bytes whatever the session's
/// binding count (FIG-6418 L2): the state a session holds never enters the
/// descriptor, so a cell that binds nothing commits a descriptor and the
/// pages its cell changed — none — not the session's names. The large
/// session holds 50,000 bindings: a cell's lowering request names every
/// binding, and the worker frame decodes at most 100,000 values.
#[tokio::test(flavor = "multi_thread")]
async fn a_no_op_cell_writes_the_same_pages_and_bytes_whatever_the_binding_count() {
    async fn session_with_bindings(host: &DurableHost, count: usize) -> CodeModeExecutionState {
        let mut state = typescript_state();
        let mut patch = lash_rlm_types::RlmGlobalsPatchPluginBody::default();
        for index in 0..count {
            patch
                .set_default
                .insert(format!("b{index}"), serde_json::json!(index));
        }
        state
            .patch_globals(&patch)
            .await
            .expect("the host's defaults apply");
        commit_working(&mut state, host).await;
        state
    }

    /// The store traffic of `code`'s cell plus its working commit.
    async fn measured(
        state: &mut CodeModeExecutionState,
        host: &DurableHost,
        key: &'static str,
        code: &str,
    ) -> lash_core::operational_metrics::sql::Receipt {
        let ((), receipt) = Box::pin(lash_core::operational_metrics::sql::collect(
            "fig6418-l2",
            "sqlite",
            async {
                cell(state, host, key, code).await;
                commit_working(state, host).await;
            },
        ))
        .await;
        receipt
    }

    let small_host = open_host().await;
    let mut small = session_with_bindings(&small_host, 1_000).await;

    // The measurement is live: a cell that binds a value installs its
    // pages, and the window sees them.
    let bound = Box::pin(measured(
        &mut small,
        &small_host,
        "exec-code:live",
        "const bound = 7;",
    ))
    .await;
    assert!(
        bound.newly_installed_graph_bytes > 0,
        "a cell that binds a value installs graph bytes: {bound:?}"
    );

    let large_host = open_host().await;
    let mut large = session_with_bindings(&large_host, 50_000).await;

    let small_noop = Box::pin(measured(
        &mut small,
        &small_host,
        "exec-code:noop",
        "await control.finish(1);",
    ))
    .await;
    let large_noop = Box::pin(measured(
        &mut large,
        &large_host,
        "exec-code:noop",
        "await control.finish(1);",
    ))
    .await;

    // A no-op cell commits its descriptor and the pages it changed; neither
    // carries the session's names. The slack is constant — envelope and
    // counter encoding drift of a commit that writes O(1) nodes — and does
    // not scale with the binding count.
    const SLACK_BYTES: u64 = 4 * 1024;
    const SLACK_ROWS: u64 = 8;
    assert!(
        large_noop
            .newly_installed_graph_bytes
            .abs_diff(small_noop.newly_installed_graph_bytes)
            <= SLACK_BYTES,
        "the no-op cell's installed bytes grow with the binding count: {} (1,000 bindings) vs {} (50,000)",
        small_noop.newly_installed_graph_bytes,
        large_noop.newly_installed_graph_bytes
    );
    assert!(
        large_noop.rows_affected.abs_diff(small_noop.rows_affected) <= SLACK_ROWS,
        "the no-op cell's store writes grow with the binding count: {} (1,000 bindings) vs {} (50,000) rows",
        small_noop.rows_affected,
        large_noop.rows_affected
    );
    println!(
        "FIG-6418 L2 bindings=1000,50000 rows_affected={},{} installed_graph_bytes={},{}",
        small_noop.rows_affected,
        large_noop.rows_affected,
        small_noop.newly_installed_graph_bytes,
        large_noop.newly_installed_graph_bytes,
    );
}

/// FIG-7084: a cancelled capture keeps the held reader and its cleanup
/// obligation. Only ended runs behind the session's independent read
/// lease can transfer cleanup into the capture's transaction; that
/// independent lease survives until capture acknowledgement.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_capture_keeps_its_readers_and_their_lease_obligations() {
    use super::super::pages::{HeldRun, SessionPageAccess};
    use lash_durable::domain::pages::{LeaseFence, RootLeaseRequest};
    use lash_vm_broker::kernel::pages::{ActivationPages, CapabilityKey, PageLimits, SharedPages};
    use std::time::Duration;

    let host = open_host().await;
    let mut state = typescript_state();
    cell(&mut state, &host, "exec-code:0", "const kept = 'reader';").await;
    commit_working(&mut state, &host).await;
    let handle = host.session_pages().expect("committed session pages");
    let lease = handle
        .store
        .acquire_root_lease(RootLeaseRequest {
            owner: handle.owner.clone(),
            expected: handle.root,
            fence: LeaseFence::Actor {
                actor: handle.actor.clone(),
                epoch: handle.epoch,
            },
            ttl_millis: 60_000,
        })
        .await
        .expect("a held run's lease");
    let services = typescript_services();
    let spools = services.workers.spools().expect("the reader stages pages");
    let stage = lash_vm_broker::kernel::next_stage_lease();
    let run: SharedPages = Arc::new(tokio::sync::Mutex::new(
        ActivationPages::new(
            handle.store.clone(),
            Some(lease.clone()),
            Duration::from_secs(20),
            CapabilityKey::fresh(),
            stage.0,
            Box::new(spools.spool(stage).unwrap()),
            Arc::new(lash_core::facade_support::SystemClock),
            Duration::from_secs(60),
            PageLimits::within(lash_vm_protocol::ProtocolBounds::standard().decode),
        )
        .owning(),
    ));
    let mut access = SessionPageAccess::default();
    access.hold(HeldRun::new(run.clone(), handle.store.clone()));
    let held = access.held();
    let cx = host.context();
    let mut tx = cx.begin().await.unwrap();
    assert!(
        !access.release_with(&mut tx, &held).await,
        "a parent still reading through the held run cannot release it in-flight"
    );
    assert!(tx.released_root_leases().is_empty());
    drop(tx);

    access.note(Some(&handle));
    let session_lease = access.lease().await.unwrap().unwrap();
    let reader = run.lock().await;
    let mut tx = cx.begin().await.unwrap();
    let mut capture = Box::pin(access.release_with(&mut tx, &held));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(capture.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(capture);
    drop(tx);
    // Frame replacement uses the same ended-run cleanup as a final
    // capture. A live page reader still blocks it, and dropping that
    // waiting replacement must keep the run and its lease owned here.
    let mut replacement = Box::pin(access.release_runs(&held));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(replacement.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(replacement);
    assert_eq!(
        access.held(),
        held,
        "the cancelled capture retains its reader"
    );
    assert_eq!(reader.owned_leases().next().unwrap().id, lease.id);
    handle
        .store
        .read_pages(&lease, &[handle.root])
        .await
        .unwrap();
    drop(reader);

    let mut tx = cx.begin().await.unwrap();
    assert!(access.release_with(&mut tx, &held).await);
    assert!(tx.released_root_leases().contains(&lease.id));
    assert!(!tx.released_root_leases().contains(&session_lease.id));
    cx.commit(tx, lash_durable::CommitLabel::new("law.capture-leases"))
        .await
        .unwrap();
    handle
        .store
        .read_pages(&session_lease, &[handle.root])
        .await
        .unwrap();
    access.released_with(&held).await;
    assert!(access.held().is_empty());
    access.release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_working_state_is_adopted_whole_and_a_cancelled_cell_never_promotes_its_capture() {
    let host = open_host().await;
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        "const kept = [1, 2];\nfunction twice(x: number) { return 2 * x; }",
    )
    .await;
    let (working, _) = sent(&commit_working(&mut source, &host).await);
    let mut state = restored(&host, &source).await;
    drop(source);

    let adopted = state
        .capture_execution_graph(ExecutionGraphPurpose::Working)
        .await
        .expect("capture");
    assert_eq!(
        root_of(&adopted),
        working,
        "the restored state is the working state"
    );
    let mut probe = host.context().begin().await.expect("open a transaction");
    assert!(
        state
            .execution_candidate(
                working,
                lash_core_store::checkpoint_graph::CommitId::mint(),
                &mut probe,
            )
            .await
            .expect("the candidate builds")
            .is_none(),
        "every page of the restored working state is already committed"
    );
    state.acknowledge_execution_state_capture().await;

    let context = cell_context(&host, SESSION, TURN, "exec-code:1", Arc::new(NoTools));
    state.shows_packages(crate::testing::packages::record(&context).await);
    let speculative = Box::pin(super::super::execute_cell(
        &mut state,
        context,
        lash_core::ExecRequest {
            code: "kept.push(3);\nconst extra = twice(21);".to_owned(),
        },
        &typescript_services(),
    ))
    .await;
    assert!(speculative.error().is_none(), "{:?}", speculative.error());
    state.mark_code_execution_response_returned();
    let root = root_of(
        &state
            .capture_execution_graph(ExecutionGraphPurpose::Working)
            .await
            .expect("capture the speculative state"),
    );
    assert_ne!(root, working, "the cell changed the heap");
    state.terminate_code_execution();

    assert_eq!(
        root_of(
            &state
                .capture_execution_graph(ExecutionGraphPurpose::Working)
                .await
                .expect("capture")
        ),
        working,
        "the cancelled cell's captured heap is not the session's"
    );
    assert!(
        !state
            .names()
            .contains(&lash_kernel_value::names::Name::new("extra")),
        "the cancelled cell's binding is gone"
    );
    let read = cell(
        &mut state,
        &host,
        "exec-code:2",
        "await control.finish([kept, twice(4)]);",
    )
    .await;
    assert_eq!(finish_of(&read), serde_json::json!([[1, 2], 8]));
}
