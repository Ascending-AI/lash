#![cfg(unix)]
#![expect(
    clippy::expect_used,
    reason = "integration test helpers fail on broken fixture assumptions"
)]
//! A pooled worker hosts one kernel machine for one execution (kernel spec
//! §9 rule 5): the parent drives it a slice at a time, answers its host
//! reads and its page exchanges, and can stop at any park and carry on in
//! another process from the checkpoint root the park sealed.

use std::sync::Arc;
use std::time::Duration;

use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::stage::{StageLeaseId, StageSpool as _};
use lash_durable::DurableError;
use lash_durable::domain::pages::{RootLease, StoredPage};
use lash_kernel_doc::heap::{PageGrant, PageRef};
use lash_kernel_doc::{Datum, EffectId, Float, Handle, Integer, Name, Timestamp, parse_document};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{Bound, End, Outcome, Request, RunError, Target};
use lash_vm_broker::kernel::MachinePark;
use lash_vm_broker::kernel::pages::{
    ActivationPages, CapabilityKey, LeasedReads, PageExchange as _, PageLimits, SharedSpool,
    SpoolRoot, answer_page,
};
use lash_vm_client::wire::{self, EndWire, OutcomeWire, ParkWire, ProjectionRead, StartWire};
use lash_vm_client::*;
use lash_vm_protocol::page::PageCapability;
use lash_vm_protocol::*;

const SLICE: u64 = 1_000_000;

/// The dialect the laws' runs are written in, which every embedding the
/// laws run under installs.
const DIALECT: &str = "typescript";

thread_local! {
    /// The dialects the host of the law running on this thread names: what
    /// its workers' embedding installs.
    static HOST_DIALECTS: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

/// Runs `law` with workers whose host names `dialects`.
fn under(dialects: &'static str, law: fn()) {
    HOST_DIALECTS.set(dialects);
    law();
}

/// Every law holds whichever dialects the host names beside the one its
/// runs are written in: with a TypeScript-only embedding, and with one of
/// TypeScript and Python (FIG-5990).
macro_rules! under_each_embedding {
    ($($law:ident),* $(,)?) => {
        $(
            mod $law {
                #[test]
                fn typescript_only() {
                    super::under("typescript", super::$law);
                }

                #[test]
                fn typescript_and_python() {
                    super::under("typescript,python", super::$law);
                }
            }
        )*
    };
}

under_each_embedding!(
    a_parked_run_carries_on_in_another_worker_without_asking_again,
    parts_that_outgrow_one_frame_cross_apart_and_the_run_goes_on_the_same,
    a_typescript_cell_lowered_in_the_worker_parks_and_resumes,
    a_parked_run_is_refused_under_another_document,
    host_reads_are_answered_by_the_parent_and_prints_arrive_in_order,
    an_observed_run_names_its_last_entered_site_on_its_next_frame,
    a_run_cancel_is_observed_where_a_slice_returns,
    a_passed_bound_is_the_runs_typed_end_and_the_worker_is_reused,
    a_refusing_parent_limiter_ends_the_run_where_no_limiter_does,
    a_limit_the_parent_grows_is_saved_and_a_resumed_run_keeps_it,
    worker_crash_mid_run_leaves_the_parent_running,
    a_run_that_holds_its_worker_past_its_wall_time_ends_with_the_wall_limit,
    a_runs_own_limit_spends_nothing_of_the_restart_window,
    a_sigxcpu_lash_did_not_cause_is_a_crash_the_host_retries,
    a_forbidden_syscall_kills_the_worker_with_typed_evidence,
    a_panic_with_a_backtrace_requested_is_typed_not_a_forbidden_syscall,
    the_address_space_ceiling_bounds_a_runs_allocation,
    workers_refuse_another_protocol_before_guest_admission,
    a_worker_silent_during_bootstrap_reports_the_time_it_was_silent,
);

fn config(mode: &str) -> PoolConfig {
    let mut entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker-fixture"));
    // The fixture reads its mode first.
    entry.args.push(mode.into());
    match HOST_DIALECTS.get() {
        "typescript" => entry.args.push("--dialects=typescript".into()),
        "typescript,python" => {}
        other => panic!("a law runs under a named embedding, not `{other}`"),
    }
    // The shipped watchdog: it also bounds a worker's startup.
    let mut config = PoolConfig::standard(entry);
    config.max_workers = 1;
    config
}

fn checkout(pool: &WorkerPool) -> Checkout {
    pool.checkout(
        4 * 1024 * 1024 + FRAME_HEADER_BYTES,
        OwnerEpoch(1),
        FrameEpoch(1),
        ExecutionBudget::default(),
    )
    .expect("checkout")
}

fn owner() -> VmOwner {
    VmOwner::new("session-A")
}

/// `body` as a document against the registry every worker assembles, with
/// its identity and its wire payload.
fn document(body: &str) -> (String, EncodedPayload) {
    let embedding = lash_vm_worker::built_dialects(&WorkerTuning::standard())
        .expect("an embedding of every built dialect");
    let mut text = String::from("numbers by_spelling\nkernel 1\neffect echo(x?: Any) -> Any\n");
    for (name, id) in embedding.library().iter() {
        if ["num.add", "num.lt", "text.repeat"].contains(&name.to_string().as_str()) {
            text.push_str(&format!("use {name} = @{id}\n"));
        }
    }
    text.push_str(body);
    let document = parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
    (
        document.identity().to_string(),
        EncodedPayload(document.canonical_bytes()),
    )
}

fn bounds() -> RunBounds {
    PoolConfig::standard(WorkerEntry::helper("unused")).run_bounds
}

fn fresh(document: &EncodedPayload, target: Target, args: Vec<Datum>) -> Start {
    Start {
        owner: owner(),
        dialect: DIALECT.into(),
        document: document.clone(),
        from: StartFrom::Fresh(
            wire::encode(
                PayloadKind::Start,
                &StartWire::of(
                    target,
                    args,
                    None,
                    &lash_kernel_vm::EffectModes::recorded(),
                    Vec::new(),
                ),
            )
            .expect("start"),
        ),
        bounds: bounds(),
        binding: BINDING,
        save: None,
        warm: Vec::new(),
        observe_sites: false,
        observe_counts: false,
        observe_path: false,
        memory_limiter: false,
        charge_limiter: false,
    }
}

/// The run resumed from `root`, the checkpoint root a park sealed, with the
/// admission of each wait it stands on.
fn resumed(
    dialect: &str,
    document: &EncodedPayload,
    root: PageCapability,
    admissions: Vec<EncodedPayload>,
) -> Start {
    Start {
        owner: owner(),
        dialect: dialect.into(),
        document: document.clone(),
        from: StartFrom::Checkpoint(CheckpointStart { root, admissions }),
        bounds: bounds(),
        binding: BINDING,
        save: None,
        warm: Vec::new(),
        observe_sites: false,
        observe_counts: false,
        observe_path: false,
        memory_limiter: false,
        charge_limiter: false,
    }
}

/// The owner binding every run here is bound to: the parent stages the
/// node, the worker names its digest in each root it seals.
const BINDING: [u8; 32] = [5; 32];

/// A store the laws' page service never reads: nothing here commits, so a
/// run reads only what its activation staged.
struct Unread;

#[async_trait::async_trait]
impl LeasedReads for Unread {
    async fn read_pages(
        &self,
        _lease: &RootLease,
        _pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        panic!("nothing committed is read")
    }
    async fn renew_root_lease(&self, _lease: &RootLease) -> Result<RootLease, DurableError> {
        panic!("no lease is held")
    }
}

/// The parent's page service for one execution: every activation of it
/// seals into one spool, and a worker that resumes it reads the checkpoint
/// root its park sealed under the stage capability the park returned.
struct Parent {
    runtime: tokio::runtime::Runtime,
    pages: ActivationPages<Unread>,
    spool: SharedSpool,
    _spools: tempfile::TempDir,
    /// The page exchanges the parent answered.
    replies: u64,
    /// The stage capability the parent issued for each page a hand-over
    /// frame's seal batch carried.
    grants: std::collections::HashMap<[u8; 32], PageCapability>,
}

impl Parent {
    /// The page service of runs pinning `dialect`'s helper release.
    fn new(dialect: &str) -> Self {
        let directory = tempfile::tempdir().expect("spool directory");
        let spools = SpoolRoot::open(directory.path(), 1 << 30).expect("spools");
        let spool = SharedSpool::new(spools.spool(StageLeaseId(1)).expect("spool"));
        let releases = lash_vm_broker::kernel::state::release_manifests().expect("releases");
        let release = releases
            .manifests
            .current(dialect)
            .expect("the dialect's helper release");
        let pages = ActivationPages::new(
            Arc::new(Unread),
            None,
            Duration::from_secs(20),
            CapabilityKey::fresh(),
            1,
            Box::new(spool.clone()),
            Arc::new(lash_core_execution::runtime::SystemClock),
            Duration::from_secs(600),
            PageLimits::within(
                PoolConfig::standard(WorkerEntry::helper("unused"))
                    .protocol
                    .decode,
            ),
        )
        .with_references([NodeId(release.0), NodeId(BINDING)]);
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime"),
            pages,
            spool,
            _spools: directory,
            replies: 0,
            grants: std::collections::HashMap::new(),
        }
    }

    /// `step`, past the page exchanges it asks the parent to answer.
    fn answer(
        &mut self,
        worker: &mut Checkout,
        step: Result<RunStep, PoolError>,
    ) -> Result<RunStep, PoolError> {
        let mut step = step?;
        while let RunStep::Pages { id, request } = step {
            let reply = self.runtime.block_on(self.pages.serve(request));
            self.replies += 1;
            step = worker.page_reply(id, reply)?;
        }
        // The batch a hand-over frame carried: admitted, and its grants
        // handed to the run's next slice.
        if let RunStep::Parked {
            seal: Some(batch), ..
        }
        | RunStep::Ended {
            seal: Some(batch), ..
        }
        | RunStep::Sealed {
            seal: Some(batch), ..
        } = &step
        {
            let sealed = self
                .runtime
                .block_on(self.pages.admit(batch.clone()))
                .expect("the parent admits the batch");
            self.grants.extend(
                sealed
                    .iter()
                    .map(|capability| (capability.page, *capability)),
            );
            worker.grant(sealed);
        }
        Ok(step)
    }

    /// The stage capability the parent issued for the page `id` a run
    /// sealed.
    fn granted(&self, id: [u8; 32]) -> PageCapability {
        *self.grants.get(&id).expect("the parent granted the page")
    }

    /// One slice of `worker`'s run.
    fn run(
        &mut self,
        worker: &mut Checkout,
        slice: u64,
        cancel: bool,
    ) -> Result<RunStep, PoolError> {
        let step = worker.run(slice, cancel);
        self.answer(worker, step)
    }

    /// The value the session a finished cell carries binds `name` to,
    /// read from the pages its run staged.
    fn carried(&self, end: &EndWire, name: &str) -> Option<Datum> {
        let EndWire::Finished {
            carry: Some(carry), ..
        } = end
        else {
            panic!("the cell finishes with a carry: {end:?}")
        };
        let session = carry
            .session(|_| Ok(PageGrant::NONE))
            .expect("the carry decodes");
        let mut editor = lash_kernel_vm::SessionEditor::open(
            Some(&session),
            Box::new(Staged(self.spool.clone())),
        )
        .expect("the session opens");
        match editor.get(&Name::new(name)).expect("the binding reads") {
            Some(lash_kernel_vm::SessionValue::Data(value)) => Some(value),
            Some(other) => panic!("`{name}` is not data: {other:?}"),
            None => None,
        }
    }
}

/// The pages a run staged, read back.
struct Staged(SharedSpool);

impl PageSource for Staged {
    fn read(
        &mut self,
        pages: &[PageRef],
    ) -> Result<
        Vec<Result<PageAnswer, lash_kernel_doc::heap::HeapFault>>,
        lash_kernel_doc::heap::HeapFault,
    > {
        Ok(pages
            .iter()
            .map(|page| {
                let bytes = self
                    .0
                    .read(&NodeId(page.id.0))
                    .map_err(|_| lash_kernel_doc::heap::HeapFault::Missing { page: page.id })?;
                answer_page(*page, &bytes, |_| Ok(PageGrant::NONE))
            })
            .collect())
    }

    fn seal(&mut self, _page: PageBody) -> Result<PageRef, lash_kernel_doc::heap::HeapFault> {
        Err(lash_kernel_doc::heap::HeapFault::Malformed(
            "a read-back seals nothing".into(),
        ))
    }
}

/// The one effect the park requests: its wait and its argument.
fn requested(step: RunStep) -> (lash_kernel_doc::WaitId, Datum) {
    let (wait, argument, _, _) = admitted(step);
    (wait, argument)
}

/// The one effect the park requests, as the parent's admission freezes it,
/// and the checkpoint root the park sealed.
fn admitted(step: RunStep) -> (lash_kernel_doc::WaitId, Datum, EncodedPayload, [u8; 32]) {
    let RunStep::Parked {
        park, candidate, ..
    } = step
    else {
        panic!("the run parks: {step:?}")
    };
    let park: ParkWire = wire::decode(PayloadKind::Park, &park).expect("park");
    let park = MachinePark::try_from(park).expect("canonical park");
    let [Request::Effect(request)] = &park.requests[..] else {
        panic!("one effect: {park:?}")
    };
    assert_eq!(request.effect.to_string(), "echo");
    let admission = lash_vm_broker::kernel::state::admission(&lash_kernel_state::AdmittedWait {
        identity: request.identity.clone(),
        request: lash_kernel_state::AdmittedRequest::Effect {
            effect: request.effect.clone(),
            args: request.args.clone(),
        },
    });
    (
        request.wait,
        request.args[0].clone(),
        admission,
        candidate.expect("the park seals its checkpoint root"),
    )
}

fn completed(value: Datum) -> EncodedPayload {
    wire::encode(
        PayloadKind::Outcome,
        &OutcomeWire::from(Outcome::Completed(value)),
    )
    .expect("outcome")
}

/// How the run ended, as it crossed.
fn ended_wire(step: RunStep) -> EndWire {
    let RunStep::Ended { end, .. } = step else {
        panic!("the run ends: {step:?}")
    };
    wire::decode(PayloadKind::End, &end).expect("end")
}

fn ended(step: RunStep) -> End {
    ended_wire(step)
        .into_end(|_| Ok(PageGrant::NONE))
        .expect("the end reads back")
}

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}

const TOOL_LOOP: &str = r#"
main {
  let total = 0
  for step in [1, 2, 3] {
    let got = perform echo(step) as Any
    set total = num.add(total, got)
  }
  return total
}"#;

/// Runs `document`, which performs `echo` three times in turn, answering
/// each with its argument: the worker is killed at the second park, and
/// another worker carries on from the checkpoint root that park sealed.
/// Returns how the run ended and every argument `echo` was asked with.
fn killed_at_a_park_and_resumed(
    pool: &WorkerPool,
    parent: &mut Parent,
    dialect: &str,
    identity: &str,
    document: &EncodedPayload,
) -> (EndWire, Vec<Datum>) {
    let mut asked = Vec::new();
    let mut first = checkout(pool);
    let pid = first.pid().expect("pid");
    let mut start = fresh(document, Target::Main, Vec::new());
    start.dialect = dialect.into();
    first
        .start(start, identity, ExecutionClass::Cell)
        .expect("start");
    let (wait, argument) = requested(parent.run(&mut first, SLICE, false).expect("run"));
    asked.push(argument.clone());
    first.deliver(wait, completed(argument)).expect("deliver");
    let (wait, argument, admission, root) =
        admitted(parent.run(&mut first, SLICE, false).expect("run"));
    let root = parent.granted(root);
    asked.push(argument.clone());
    // Killed at the park: the worker is discarded, not reset.
    drop(first);

    let mut second = checkout(pool);
    assert_ne!(second.pid(), Some(pid));
    second
        .start(
            resumed(dialect, document, root, vec![admission]),
            identity,
            ExecutionClass::Cell,
        )
        .expect("resume");
    second.deliver(wait, completed(argument)).expect("deliver");
    let end = loop {
        let step = parent.run(&mut second, SLICE, false).expect("run");
        if !matches!(step, RunStep::Parked { .. }) {
            break ended_wire(step);
        }
        let (wait, argument) = requested(step);
        asked.push(argument.clone());
        second.deliver(wait, completed(argument)).expect("deliver");
    };
    second
        .release()
        .expect("the worker resets at the run's end");
    (end, asked)
}

/// Targets 1 and 2: a run that loops over effects parks at each, sealing
/// its checkpoint root; its worker is killed, and another worker carries
/// on from the root. The effect answered before the kill is not asked for
/// again.
fn a_parked_run_carries_on_in_another_worker_without_asking_again() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut parent = Parent::new(DIALECT);
    let (identity, document) = document(TOOL_LOOP);
    let (end, asked) =
        killed_at_a_park_and_resumed(&pool, &mut parent, DIALECT, &identity, &document);
    let EndWire::Finished { result, .. } = end else {
        panic!("the run finishes: {end:?}")
    };
    assert_eq!(result, int(6));
    assert_eq!(asked, [int(1), int(2), int(3)], "each effect is asked once");
}

/// FIG-5921: a run's start crosses with its first slice and the outcomes it
/// waits on with its next, each in one frame. Parts that outgrow one frame
/// together cross apart, and the run goes on the same: a resumed run's
/// start and a large outcome each cross alone before the slice. A park's
/// state never crosses: it is pages the parent's service staged.
fn parts_that_outgrow_one_frame_cross_apart_and_the_run_goes_on_the_same() {
    const FRAME: u32 = 64 * 1024;
    let mut config = config("");
    config.protocol.decode.max_frame_bytes = FRAME;
    let pool = WorkerPool::new(config).expect("pool");
    let mut parent = Parent::new(DIALECT);
    let sent = || pool.measurements().counters.ipc_sent_messages;
    // Never run: it makes the document, and so each start, large.
    let padding = "p".repeat(30 * 1024);
    let (identity, document) = document(&format!(
        r#"
main {{
  if num.lt(1, 0) {{ let unused = "{padding}" }}
  let kept = perform echo("a") as Any
  let again = perform echo(kept) as Any
  return again
}}"#
    ));
    let large = Datum::Text("y".repeat(40 * 1024));

    let mut first = checkout(&pool);
    first
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, _, admission, root) = admitted(parent.run(&mut first, SLICE, false).expect("run"));
    let root = parent.granted(root);
    drop(first);

    let mut second = checkout(&pool);
    second
        .start(
            resumed(DIALECT, &document, root, vec![admission]),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("resume");
    second
        .deliver(wait, completed(large.clone()))
        .expect("deliver");
    let before = (sent(), parent.replies);
    let (_, argument) = requested(parent.run(&mut second, SLICE, false).expect("run"));
    assert_eq!(argument, large, "the large outcome was delivered");
    assert_eq!(
        (sent() - before.0) - (parent.replies - before.1),
        3,
        "the start, the outcome and the slice cross apart"
    );
    second.release().expect("release at the park");
}

/// Targets 1 and 5: a TypeScript cell is lowered in the worker against the
/// functions it registered at startup, and the document it gives runs,
/// parks, is killed and resumes like any other. The cell's top-level
/// binding is the session's when the run finishes.
fn a_typescript_cell_lowered_in_the_worker_parks_and_resumes() {
    use lash_vm_client::service::{Request, Response};
    let pool = WorkerPool::new(config("")).expect("pool");
    let echo = lash_kernel_doc::Signature {
        params: vec![lash_kernel_doc::Param {
            name: Name::new("x"),
            ty: lash_kernel_doc::Type::Any,
            optional: false,
        }],
        result: lash_kernel_doc::Type::Any,
    };
    let request = Request::Lower {
        dialect: "typescript".into(),
        source:
            "let total = 0; for (const step of [1, 2, 3]) { total = total + await echo(step); }"
                .into(),
        effects: [(
            lash_kernel_doc::EffectName::new("echo").expect("effect name"),
            echo,
        )]
        .into(),
        controls: Default::default(),
        tool_roots: Default::default(),
        bindings: Default::default(),
        functions: Default::default(),
        helpers: lash_vm_library::HELPER_RELEASE,
        reads: Default::default(),
    };
    let mut worker = checkout(&pool);
    let response = worker
        .prepare(
            owner(),
            EncodedPayload(rmp_serde::to_vec_named(&request).expect("request")),
        )
        .expect("lowered in the worker");
    worker.release().expect("reset");
    let Response::Lowered { document, .. } = rmp_serde::from_slice(&response.0).expect("response")
    else {
        panic!("the cell lowers")
    };
    let identity = lash_kernel_doc::IdentifiedDocument::decode_standard(&document)
        .expect("document")
        .id()
        .to_string();
    let document = EncodedPayload(document);
    let mut parent = Parent::new(DIALECT);
    let (end, asked) =
        killed_at_a_park_and_resumed(&pool, &mut parent, DIALECT, &identity, &document);
    assert_eq!(asked.len(), 3, "each call is asked once: {asked:?}");
    assert_eq!(
        parent.carried(&end, "total"),
        Some(Datum::Float(Float::new(6.0)))
    );
}

/// FIG-5990: a run names the dialect its session or process recorded at
/// creation, and a worker whose host did not name that dialect refuses it
/// with a typed refusal before it reads the document or runs anything: a
/// new run before its first slice, and a parked one before its state. The
/// worker of a host that named the dialect runs the same start.
#[test]
fn a_worker_refuses_a_run_in_a_dialect_its_host_did_not_name() {
    let (identity, document) = {
        HOST_DIALECTS.set("typescript,python");
        document(TOOL_LOOP)
    };
    let python = |from: StartFrom| Start {
        dialect: "python".into(),
        from,
        ..fresh(&document, Target::Main, Vec::new())
    };
    let new_run = || python(fresh(&document, Target::Main, Vec::new()).from);

    // A host that names Python runs it, to a park another worker resumes.
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut parent = Parent::new("python");
    let mut worker = checkout(&pool);
    worker
        .start(new_run(), &identity, ExecutionClass::Cell)
        .expect("start");
    let (_, _, admission, root) =
        admitted(parent.run(&mut worker, SLICE, false).expect("a Python run"));
    let root = parent.granted(root);
    worker.release().expect("release at the park");
    drop(pool);

    // A host that names TypeScript alone refuses both starts.
    HOST_DIALECTS.set("typescript");
    let pool = WorkerPool::new(config("")).expect("pool");
    let parked = resumed("python", &document, root, vec![admission]);
    for (start, from) in [(new_run(), "a new run"), (parked, "a parked run")] {
        let mut worker = checkout(&pool);
        let refused = worker
            .start(start, &identity, ExecutionClass::Cell)
            .and_then(|()| parent.run(&mut worker, SLICE, false))
            .expect_err("the worker installs no Python");
        assert!(
            matches!(
                &refused,
                PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                    refusal: RunRefusal::DialectNotInstalled { dialect }
                }) if dialect == "python"
            ),
            "{from}: {refused:?}"
        );
    }
}

/// FIG-5990: a Python cell is lowered and run by the worker of a host that
/// names Python, through the entry and the confinement every worker has:
/// lowering it, running it and parking at each effect makes no system call
/// the confinement forbids, and the run pins Python's helper release, on
/// which another worker resumes it.
#[test]
fn a_python_cell_lowered_in_the_worker_runs_under_its_confinement() {
    use lash_vm_client::service::{Request, Response};
    HOST_DIALECTS.set("typescript,python");
    let pool = WorkerPool::new(config("")).expect("pool");
    let echo = lash_kernel_doc::Signature {
        params: vec![lash_kernel_doc::Param {
            name: Name::new("x"),
            ty: lash_kernel_doc::Type::Any,
            optional: false,
        }],
        result: lash_kernel_doc::Type::Any,
    };
    let request = Request::Lower {
        dialect: "python".into(),
        source: "total = 0\nfor step in [1, 2, 3]:\n    total = total + await echo(step)\n".into(),
        effects: [(
            lash_kernel_doc::EffectName::new("echo").expect("effect name"),
            echo,
        )]
        .into(),
        controls: Default::default(),
        tool_roots: Default::default(),
        bindings: Default::default(),
        functions: Default::default(),
        helpers: lash_vm_library::HELPER_RELEASE,
        reads: Default::default(),
    };
    let mut worker = checkout(&pool);
    let response = worker
        .prepare(
            owner(),
            EncodedPayload(rmp_serde::to_vec_named(&request).expect("request")),
        )
        .expect("lowered in the worker");
    let Response::Lowered { document, .. } = rmp_serde::from_slice(&response.0).expect("response")
    else {
        panic!("the cell lowers")
    };
    let identity = lash_kernel_doc::IdentifiedDocument::decode_standard(&document)
        .expect("document")
        .id()
        .to_string();
    let document = EncodedPayload(document);
    worker.release().expect("reset");
    // Its first park seals its root and the worker is killed: another
    // resumes it only on the release it pinned, which holds Python's
    // helpers and no other dialect's.
    let mut parent = Parent::new("python");
    let (end, asked) =
        killed_at_a_park_and_resumed(&pool, &mut parent, "python", &identity, &document);
    assert_eq!(asked.len(), 3, "each call is asked once");
    assert_eq!(parent.carried(&end, "total"), Some(int(6)));
}

/// A parked run names its document; a worker handed another document
/// refuses the state before any machine is built.
fn a_parked_run_is_refused_under_another_document() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut parent = Parent::new(DIALECT);
    let (identity, document_a) = document(TOOL_LOOP);
    let (other, document_b) = document("main { let got = perform echo(1) as Any return got }");
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(&document_a, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (_, _, admission, root) = admitted(parent.run(&mut worker, SLICE, false).expect("run"));
    let root = parent.granted(root);
    worker.release().expect("release at the park");

    let mut next = checkout(&pool);
    let refused = next
        .start(
            resumed(DIALECT, &document_b, root, vec![admission]),
            &other,
            ExecutionClass::Cell,
        )
        .and_then(|()| parent.run(&mut next, SLICE, false))
        .expect_err("another document's root");
    assert!(
        matches!(
            refused,
            PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                refusal: RunRefusal::Restore {
                    refusal: KernelRestoreRefusal::DocumentMismatch { .. },
                }
            })
        ),
        "{refused:?}"
    );
}

/// FIG-6343: a run whose parent observes sites reports them on the run
/// frames it already sends. Entering a site is no host read: the next frame
/// names each task's last entered site, and a frame after no entry names
/// none.
fn an_observed_run_names_its_last_entered_site_on_its_next_frame() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document(
        r#"
entry go(n: Any) -> Any
fn same(value) { return value }
fn go(n) {
  for i in [1, 2] { let v = call same(i) }
  let got = perform echo(n) as Any
  return got
}
main { }"#,
    );
    let mut worker = checkout(&pool);
    let mut start = fresh(&document, Target::Entry(Name::new("go")), vec![int(7)]);
    start.observe_sites = true;
    worker
        .start(start, &identity, ExecutionClass::Process)
        .expect("start");
    let mut parent = Parent::new(DIALECT);
    let (wait, argument) = requested(parent.run(&mut worker, SLICE, false).expect("run"));
    let entered: Vec<Vec<EffectId>> = worker
        .take_sites()
        .iter()
        .map(|payload| {
            wire::decode::<wire::Sites>(PayloadKind::Sites, payload)
                .expect("sites")
                .entered
        })
        .collect();
    let [entered] = &entered[..] else {
        panic!("the park alone carries the sites: {entered:?}")
    };
    let [call] = &entered[..] else {
        panic!("the task's last entry alone: {entered:?}")
    };
    assert_eq!(
        call.visit(),
        lash_protocol_types::workflow_testkit::visit(1),
        "the second call is the last entry"
    );
    assert_eq!(
        lash_kernel_doc::ObservationReader::counts(call)
            .loops
            .iter()
            .map(|iteration| iteration.entries)
            .collect::<Vec<_>>(),
        [2]
    );
    worker.deliver(wait, completed(argument)).expect("deliver");
    let end = ended(parent.run(&mut worker, SLICE, false).expect("run"));
    assert!(
        worker.take_sites().is_empty(),
        "no site was entered after the park"
    );
    let End::Finished(finished) = end else {
        panic!("the run finishes: {end:?}")
    };
    assert_eq!(finished.result, int(7));
}

/// Target 1: the machine's clock, random source and projection reads are
/// answered by the parent in the middle of a slice, and what the run prints
/// arrives with the slice, in order.
fn host_reads_are_answered_by_the_parent_and_prints_arrive_in_order() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document(
        r#"
entry go(h: Handle("table")) -> Any
fn go(h) {
  print "before"
  let row = read(h, "row 7")
  print row
  return (clock, random, row)
}
main { }"#,
    );
    let handle = Handle {
        kind: "table".into(),
        id: "t1".into(),
    };
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(
                &document,
                Target::Entry(Name::new("go")),
                vec![Datum::Handle(handle.clone())],
            ),
            &identity,
            ExecutionClass::Process,
        )
        .expect("start");
    let now = Timestamp {
        nanoseconds: Integer::from(1_700_000_000),
    };
    let mut parent = Parent::new(DIALECT);
    let mut kinds = Vec::new();
    let mut step = parent.run(&mut worker, SLICE, false).expect("run");
    let end = loop {
        match step {
            RunStep::HostRead { id, kind, request } => {
                kinds.push(kind);
                let answer = match kind {
                    HostReadKind::Projection => {
                        let read: ProjectionRead =
                            wire::decode(PayloadKind::HostRead, &request).expect("read");
                        assert_eq!(read.handle, handle);
                        assert_eq!(read.request, Datum::Text("row 7".into()));
                        let answer: wire::ProjectionAnswer = Ok(Datum::Text("seven".into()));
                        wire::encode(PayloadKind::HostAnswer, &answer)
                    }
                    HostReadKind::Clock => wire::encode(PayloadKind::HostAnswer, &now),
                    // The top 53 bits, times 2^-53: one half.
                    HostReadKind::Random => wire::encode(PayloadKind::HostAnswer, &(1_u64 << 63)),
                    HostReadKind::Effect | HostReadKind::Bound => {
                        panic!("the run binds no read and has no limiter")
                    }
                }
                .expect("answer");
                let answered = worker.host_answer(id, answer);
                step = parent.answer(&mut worker, answered).expect("answered");
            }
            other => break ended(other),
        }
    };
    assert_eq!(
        kinds,
        [
            HostReadKind::Projection,
            HostReadKind::Clock,
            HostReadKind::Random
        ]
    );
    let printed: Vec<Datum> = worker
        .take_printed()
        .iter()
        .flat_map(|payload| {
            rmp_serde::from_slice::<Vec<serde_bytes::ByteBuf>>(&payload.0).expect("chunk")
        })
        .map(|value| wire::from_json(PayloadKind::Printed, &value).expect("value"))
        .collect();
    assert_eq!(
        printed,
        [Datum::Text("before".into()), Datum::Text("seven".into())]
    );
    let End::Finished(finished) = end else {
        panic!("the run finishes: {end:?}")
    };
    assert_eq!(
        finished.result,
        Datum::Tuple(vec![
            Datum::Timestamp(now),
            Datum::Float(Float::new(0.5)),
            Datum::Text("seven".into())
        ])
    );
    worker.release().expect("reset");
}

/// Target 2: a slice returns to the parent with the run still ready, and a
/// run cancel sent with the next slice ends the run cancelled. The worker
/// is whole and resets.
fn a_run_cancel_is_observed_where_a_slice_returns() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document("main { while true { } }");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    assert!(matches!(
        worker.run(1_000, false).expect("run"),
        RunStep::Slice { .. }
    ));
    assert_eq!(ended(worker.run(1_000, true).expect("run")), End::Cancelled);
    worker.release().expect("reset");
    assert_eq!(checkout(&pool).pid(), Some(pid), "the worker is reused");
}

/// A run that passes a bound ends with that bound's typed error. It is the
/// run's own end, not a worker failure: the worker resets and is reused.
fn a_passed_bound_is_the_runs_typed_end_and_the_worker_is_reused() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document("main { while true { } }");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.bounds.charge_limit = 500;
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("start");
    let End::Error(RunError::Bound(exceeded)) = ended(worker.run(SLICE, false).expect("run"))
    else {
        panic!("the charge bound ends the run")
    };
    assert_eq!(exceeded.bound, Bound::Charge);
    assert_eq!(exceeded.limit, 500);
    worker.release().expect("reset");
    assert_eq!(checkout(&pool).pid(), Some(pid));
}

/// `step`, past the bound reads it asks the parent: each is answered from
/// `answers`, then refused, and kept in `asked`.
fn limited(
    parent: &mut Parent,
    worker: &mut Checkout,
    mut step: RunStep,
    answers: &mut std::collections::VecDeque<LimitDecision>,
    asked: &mut Vec<BoundReached>,
) -> RunStep {
    while let RunStep::HostRead {
        id,
        kind: HostReadKind::Bound,
        request,
    } = step
    {
        asked.push(wire::decode(PayloadKind::HostRead, &request).expect("a bound read"));
        let decision = answers.pop_front().unwrap_or_default();
        let answer = wire::encode(PayloadKind::HostAnswer, &decision).expect("answer");
        let answered = worker.host_answer(id, answer);
        step = parent.answer(worker, answered).expect("answered");
    }
    step
}

/// The meters a run crossed with.
fn metered(step: &RunStep) -> RunMeters {
    match step {
        RunStep::Parked { meters, .. }
        | RunStep::Slice { meters }
        | RunStep::Ended { meters, .. } => *meters,
        other => panic!("the run stops: {other:?}"),
    }
}

/// FIG-6768 law 4, worker host: a parent whose limiter refuses ends the run
/// with the `BoundExceeded` a run whose parent has no limiter ends with, at
/// the same point and charged the same, for both bounds. The worker asked
/// once, naming the bound, what the run used and its limit.
fn a_refusing_parent_limiter_ends_the_run_where_no_limiter_does() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut parent = Parent::new(DIALECT);
    let charge = document("main { while true { } }");
    let memory = document(
        r#"
main {
  let big = text.repeat("x", 200000)
  return 0
}"#,
    );
    for (bound, (identity, document)) in [(Bound::Charge, charge), (Bound::Memory, memory)] {
        let mut ends = Vec::new();
        for limiter in [true, false] {
            let mut worker = checkout(&pool);
            let mut start = fresh(&document, Target::Main, Vec::new());
            start.bounds.charge_limit = 500;
            start.bounds.memory = 64 << 10;
            start.charge_limiter = limiter;
            start.memory_limiter = limiter;
            worker
                .start(start, &identity, ExecutionClass::Cell)
                .expect("start");
            let mut asked = Vec::new();
            let step = parent.run(&mut worker, SLICE, false).expect("run");
            let step = limited(
                &mut parent,
                &mut worker,
                step,
                &mut Default::default(),
                &mut asked,
            );
            let meters = metered(&step);
            let End::Error(RunError::Bound(exceeded)) = ended(step) else {
                panic!("{bound:?}: the bound ends the run")
            };
            assert_eq!(exceeded.bound, bound);
            let limit = match bound {
                Bound::Charge => 500,
                _ => 64 << 10,
            };
            assert_eq!(exceeded.limit, limit);
            match limiter {
                true => {
                    let [reached] = asked[..] else {
                        panic!("{bound:?}: asked once: {asked:?}")
                    };
                    assert_eq!(reached.limit, limit);
                    assert!(reached.used > limit, "{reached:?}");
                    assert_eq!(
                        reached.bound,
                        match bound {
                            Bound::Charge => LimitedBound::Charge,
                            _ => LimitedBound::Memory,
                        }
                    );
                }
                false => assert_eq!(asked, [], "a parent without a limiter is not asked"),
            }
            ends.push((exceeded, meters));
            worker.release().expect("reset");
        }
        assert_eq!(ends[0], ends[1], "{bound:?}");
    }
}

/// FIG-6768 law 1, worker host: the parent grows the run's charge limit
/// where the run reaches it; the park saves the grown limit, the worker is
/// killed there, and another worker started with the limit the run began
/// with carries on with the grown one, unasked. The run ends as one
/// started with the grown limit does, charged the same.
fn a_limit_the_parent_grows_is_saved_and_a_resumed_run_keeps_it() {
    const LIMIT: u64 = 300;
    const GROW: u64 = 100_000;
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut parent = Parent::new(DIALECT);
    let (identity, document) = document(
        r#"
main {
  let i = 0
  while num.lt(i, 200) {
    set i = num.add(i, 1)
  }
  let got = perform echo(i) as Any
  let j = 0
  while num.lt(j, 200) {
    set j = num.add(j, 1)
  }
  return num.add(got, j)
}"#,
    );
    let mut ran = Vec::new();
    for (limit, limiter) in [(LIMIT, true), (LIMIT + GROW, false)] {
        let mut answers = std::collections::VecDeque::from([LimitDecision::Grow(GROW)]);
        let mut asked = Vec::new();
        let mut first = checkout(&pool);
        let mut start = fresh(&document, Target::Main, Vec::new());
        start.bounds.charge_limit = limit;
        start.charge_limiter = limiter;
        first
            .start(start, &identity, ExecutionClass::Cell)
            .expect("start");
        let step = parent.run(&mut first, SLICE, false).expect("run");
        let step = limited(&mut parent, &mut first, step, &mut answers, &mut asked);
        let parked = metered(&step);
        let (wait, argument, admission, root) = admitted(step);
        let root = parent.granted(root);
        drop(first);
        let before = asked.len();

        let mut second = checkout(&pool);
        let mut start = resumed(DIALECT, &document, root, vec![admission]);
        start.bounds.charge_limit = limit;
        start.charge_limiter = limiter;
        second
            .start(start, &identity, ExecutionClass::Cell)
            .expect("resume");
        second.deliver(wait, completed(argument)).expect("deliver");
        let step = parent.run(&mut second, SLICE, false).expect("run");
        let step = limited(&mut parent, &mut second, step, &mut answers, &mut asked);
        let meters = metered(&step);
        let End::Finished(finished) = ended(step) else {
            panic!("the run finishes")
        };
        assert_eq!(finished.result, int(400));
        second.release().expect("reset");
        if limiter {
            assert_eq!(before, 1, "asked once before the park: {asked:?}");
            assert_eq!(asked.len(), 1, "the resumed run kept the grown limit");
            assert_eq!(asked[0].limit, LIMIT);
            assert_eq!(asked[0].bound, LimitedBound::Charge);
        }
        ran.push((parked, meters));
    }
    assert_eq!(ran[0], ran[1]);
}

/// A worker that dies in the middle of a run fails the exchange typed, is
/// reaped, and leaves the parent with a pool that hands out a replacement.
/// It died after it began computing the run, so it died running it: a
/// verdict another attempt may pass, which still spends one of the
/// execution's attempts (FIG-6786).
fn worker_crash_mid_run_leaves_the_parent_running() {
    let pool = WorkerPool::new(config("abort")).expect("pool");
    let mut parent = Parent::new(DIALECT);
    let (identity, document) = document(TOOL_LOOP);
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, argument) = requested(parent.run(&mut worker, SLICE, false).expect("run"));
    worker
        .deliver(wait, completed(argument))
        .expect("the outcome waits for the next slice");
    let lost = parent.run(&mut worker, SLICE, false).err();
    assert!(
        matches!(
            lost,
            Some(PoolError::Infrastructure(
                InfrastructureOutcome::RunCrashedWorker { .. }
            ))
        ),
        "the worker died running the run: {lost:?}"
    );
    assert!(lost.is_some_and(|lost| lost.is_host_verdict()));
    let next = checkout(&pool);
    assert_ne!(next.pid(), Some(pid));
    next.release().expect("pristine replacement");
}

/// FIG-6786: an execution holds its workers for at most its wall time,
/// summed over its exchanges. Every slice of an endless run returns to the
/// parent well within the compute deadline, and each starts a fresh compute
/// window, yet the run ends with the wall limit once it has held its worker
/// that long: the run's own end, never the host's verdict.
fn a_run_that_holds_its_worker_past_its_wall_time_ends_with_the_wall_limit() {
    let mut config = config("");
    config.deadlines.execution_wall = Duration::from_millis(200);
    config.deadlines.cumulative_cpu = Duration::from_secs(3);
    config.run_bounds.charge_limit = u64::MAX;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document("main { while true { } }");
    let mut worker = checkout(&pool);
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.bounds.charge_limit = u64::MAX;
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("start");
    let lost = loop {
        match worker.run(10_000, false) {
            Ok(RunStep::Slice { .. }) => {}
            Ok(other) => panic!("the endless run only slices: {other:?}"),
            Err(error) => break error,
        }
    };
    assert_eq!(
        lost,
        PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
            limit: WorkerLimit::Wall { bound_ms: 200 }
        })
    );
    assert!(!lost.is_host_verdict(), "the run's own end: {lost:?}");
}

/// FIG-6786: the pool's restart window counts failures of its workers, not
/// a run's own typed limit. Twice a run passes its compute deadline and
/// loses its worker; with a window one failure fills, the pool still hands
/// out the next worker instead of `RestartStorm`.
fn a_runs_own_limit_spends_nothing_of_the_restart_window() {
    let mut config = config("");
    config.max_restarts = 1;
    config.deadlines.compute = Duration::from_millis(100);
    config.run_bounds.charge_limit = u64::MAX;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document("main { while true { } }");
    for _ in 0..2 {
        let mut worker = checkout(&pool);
        let mut start = fresh(&document, Target::Main, Vec::new());
        start.bounds.charge_limit = u64::MAX;
        worker
            .start(start, &identity, ExecutionClass::Cell)
            .expect("start");
        assert_eq!(
            worker.run(u64::MAX, false).err(),
            Some(PoolError::Infrastructure(
                InfrastructureOutcome::WorkerLimitExceeded {
                    limit: WorkerLimit::Deadline { bound_ms: 100 }
                }
            ))
        );
    }
    assert!(!pool.stats().restart_storm);
    checkout(&pool).release().expect("a fresh worker");
}

/// FIG-6523: the kernel stops a worker with `SIGXCPU` at the CPU ceiling
/// the worker set from its execution's CPU budget, which is the run's own
/// limit. A `SIGXCPU` that reaches a worker with that budget unspent is no
/// limit of lash's: it stays a crash, the host's verdict, which the host
/// retries.
fn a_sigxcpu_lash_did_not_cause_is_a_crash_the_host_retries() {
    let pool = WorkerPool::new(config("xcpu")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("the parent takes the run");
    let Err(lost) = worker.run(SLICE, false) else {
        panic!("the signalled worker answers nothing")
    };
    assert_eq!(
        lost,
        PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed {
            evidence: SupervisorEvidence::Signalled {
                signal: libc::SIGXCPU
            }
        })
    );
    assert!(lost.is_host_verdict(), "the host retries a crash: {lost:?}");
}

/// FIG-5858: a worker serves under a system-call allowlist. A call outside
/// it, such as opening a socket or mapping memory writable and executable
/// at once, kills the worker; the parent reads that as a typed crash and
/// the pool hands out a replacement.
fn a_forbidden_syscall_kills_the_worker_with_typed_evidence() {
    let (identity, document) = document(TOOL_LOOP);
    for mode in ["open_socket", "map_write_exec"] {
        let pool = WorkerPool::new(config(mode)).expect("pool");
        let mut worker = checkout(&pool);
        let pid = worker.pid().expect("pid");
        worker
            .start(
                fresh(&document, Target::Main, Vec::new()),
                &identity,
                ExecutionClass::Cell,
            )
            .expect("the parent takes the run");
        let started = worker.run(SLICE, false);
        assert!(
            matches!(
                started,
                Err(PoolError::Infrastructure(
                    InfrastructureOutcome::WorkerCrashed {
                        evidence: SupervisorEvidence::ForbiddenSyscall
                    }
                ))
            ),
            "{mode}: {started:?}"
        );
        drop(worker);
        let next = checkout(&pool);
        assert_ne!(next.pid(), Some(pid), "{mode}");
        next.release().expect("pristine replacement");
    }
}

/// FIG-5876: a worker that panics with RUST_BACKTRACE=1 reports the typed
/// panic. Printing the backtrace asks for the working directory, which the
/// confinement refuses rather than kills on, so the panic still reaches the
/// parent.
fn a_panic_with_a_backtrace_requested_is_typed_not_a_forbidden_syscall() {
    let pool = WorkerPool::new(config("panic_backtrace")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("the parent takes the run");
    let started = worker.run(SLICE, false);
    let Err(PoolError::Infrastructure(InfrastructureOutcome::ProtocolViolation {
        breach: ProtocolBreach::Panicked { detail },
    })) = started
    else {
        panic!("a typed panic: {started:?}")
    };
    assert_eq!(detail, Detail::new("a panic with a backtrace requested"));
}

/// FIG-5858: a worker's address space has a ceiling, which bounds what a
/// run can allocate even where the run's memory bound would allow more.
/// The allocation is refused as the run's memory bound, and the worker,
/// which never held the memory, is reused.
fn the_address_space_ceiling_bounds_a_runs_allocation() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut config = config("");
    config.confinement.address_space_bytes = GIB;
    // Room for the parser stack of the largest admitted source.
    config.protocol.max_source_bytes = 1024;
    config.tuning.typescript_source_bytes = 1024;
    config.run_bounds.memory = 8 * GIB;
    config.run_bounds.charge_limit = u64::MAX;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document(
        r#"
main {
  let big = text.repeat("x", 1500000000)
  return 0
}"#,
    );
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.bounds.memory = 8 * GIB;
    start.bounds.charge_limit = u64::MAX;
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("start");
    let End::Error(RunError::Bound(exceeded)) = ended(worker.run(SLICE, false).expect("run"))
    else {
        panic!("the ceiling refuses the text")
    };
    assert_eq!(exceeded.bound, Bound::Memory);
    worker.release().expect("reset");
    assert_eq!(checkout(&pool).pid(), Some(pid), "the worker is reused");
}

/// FIG-6787: a host's pool memory budget holds `n` workers when `n` times a
/// worker's ceiling and page budgets, plus the staging quota, fit in it. The
/// pool starts no worker past it: a checkout that finds every running
/// worker taken meets the pool's capacity faults, as past `max_workers`,
/// and takes a worker once one is released. A budget that holds no worker
/// is refused.
#[test]
fn the_pool_memory_budget_refuses_the_worker_that_would_exceed_it() {
    HOST_DIALECTS.set("typescript");
    let mut config = config("");
    config.max_workers = 2;
    // A checkout that finds no worker answers at once.
    config.max_queue_items = 0;
    let one = config.committed_memory(1).expect("one worker's commitment");
    let mut too_small = config.clone();
    too_small.memory_budget_bytes = Some(one - 1);
    assert!(matches!(
        WorkerPool::new(too_small),
        Err(PoolError::InvalidConfiguration)
    ));
    let two = config.committed_memory(2).expect("two workers' commitment");
    config.memory_budget_bytes = Some(two - 1);
    let pool = WorkerPool::new(config).expect("pool");
    let first = checkout(&pool);
    let pid = first.pid();
    let refused = pool.checkout(
        4 * 1024 * 1024 + FRAME_HEADER_BYTES,
        OwnerEpoch(1),
        FrameEpoch(1),
        ExecutionBudget::default(),
    );
    assert!(
        matches!(refused, Err(PoolError::QueueFull { .. })),
        "{:?}",
        refused.map(|checkout| checkout.pid())
    );
    assert_eq!(pool.stats().workers, 1, "no second worker started");
    first.release().expect("reset");
    assert_eq!(checkout(&pool).pid(), pid, "the released worker serves");
}

/// Why this host runs no cgroup law: the laws need a cgroup v2 subtree
/// delegated to them, named by `LASH_TEST_WORKER_CGROUP`.
#[derive(Debug)]
enum NoCgroup {
    /// The environment names no subtree.
    Unnamed,
    /// The subtree it names cannot hold a worker.
    Refused(CgroupRefusal),
}

impl std::fmt::Display for NoCgroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unnamed => write!(f, "unnamed: LASH_TEST_WORKER_CGROUP names no subtree"),
            Self::Refused(refusal) => write!(f, "refused: {refusal}"),
        }
    }
}

/// The subtree this host delegates to the laws, under `limits`.
#[expect(
    clippy::disallowed_methods,
    reason = "the law's host names the cgroup subtree it delegates in its environment"
)]
fn delegated(limits: impl FnOnce(&mut WorkerCgroup)) -> Result<WorkerCgroup, NoCgroup> {
    let subtree = std::env::var_os("LASH_TEST_WORKER_CGROUP").ok_or(NoCgroup::Unnamed)?;
    let mut cgroup = WorkerCgroup {
        subtree: subtree.into(),
        memory_max_bytes: 512 << 20,
        memory_high_bytes: None,
        cpu: None,
    };
    limits(&mut cgroup);
    cgroup.probe().map_err(NoCgroup::Refused)?;
    eprintln!("CGROUP_LAW_RAN subtree={}", cgroup.subtree.display());
    Ok(cgroup)
}

/// The cgroup `pid` runs in, as a directory of the cgroup v2 mount.
#[expect(
    clippy::disallowed_methods,
    reason = "the law reads where the kernel placed its worker"
)]
fn cgroup_of(pid: u32) -> std::path::PathBuf {
    let membership =
        std::fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("the worker's cgroup");
    let path = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("a cgroup v2 membership");
    std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'))
}

#[expect(
    clippy::disallowed_methods,
    reason = "the law reads the limits its worker's cgroup holds"
)]
fn cgroup_file(cgroup: &std::path::Path, file: &str) -> String {
    std::fs::read_to_string(cgroup.join(file))
        .expect("a cgroup interface file")
        .trim()
        .to_owned()
}

/// FIG-6787: a host that delegates a cgroup v2 subtree to the pool gets
/// each worker in a cgroup of its own there, entered before the worker
/// execs, holding the memory ceiling, the reclaim threshold and the CPU
/// share the host configured; the pool removes it once the worker is
/// reaped.
#[test]
fn a_worker_runs_in_a_cgroup_of_its_own_under_the_hosts_memory_and_cpu_limits() {
    let cgroup = match delegated(|cgroup| {
        cgroup.memory_high_bytes = Some(384 << 20);
        cgroup.cpu = Some(CpuShare {
            quota: Duration::from_millis(50),
            period: Duration::from_millis(100),
        });
    }) {
        Ok(cgroup) => cgroup,
        Err(reason) => {
            eprintln!("CGROUP_LAW_SKIPPED reason={reason}");
            return;
        }
    };
    HOST_DIALECTS.set("typescript");
    let mut config = config("");
    let subtree = cgroup.subtree.clone();
    config.cgroup = Some(cgroup);
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document("main { return num.add(1, 2) }");
    let mut worker = checkout(&pool);
    let placed = cgroup_of(worker.pid().expect("pid"));
    assert_eq!(placed.parent(), Some(subtree.as_path()), "{placed:?}");
    assert_eq!(
        cgroup_file(&placed, "memory.max"),
        (512_u64 << 20).to_string()
    );
    assert_eq!(
        cgroup_file(&placed, "memory.high"),
        (384_u64 << 20).to_string()
    );
    assert_eq!(cgroup_file(&placed, "cpu.max"), "50000 100000");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let End::Finished(finished) = ended(worker.run(SLICE, false).expect("run")) else {
        panic!("the placed worker serves the run")
    };
    assert_eq!(finished.result, int(3));
    worker.release().expect("reset");
    drop(pool);
    assert!(!placed.exists(), "the reaped worker's cgroup is removed");
}

/// FIG-6787: a worker whose resident memory passes its cgroup's
/// `memory.max` is killed by the kernel, and its run ends with the typed
/// memory limit it met, not a crash: the same run meets it again, so it is
/// the run's own end, never the host's to retry. Its address-space ceiling
/// and its logical memory bound are far above the ceiling, so only the
/// cgroup stops it.
#[test]
fn a_worker_past_its_cgroup_memory_max_ends_its_run_with_the_typed_memory_limit() {
    const GIB: u64 = 1024 * 1024 * 1024;
    const CEILING: u64 = 256 << 20;
    let cgroup = match delegated(|cgroup| cgroup.memory_max_bytes = CEILING) {
        Ok(cgroup) => cgroup,
        Err(reason) => {
            eprintln!("CGROUP_LAW_SKIPPED reason={reason}");
            return;
        }
    };
    HOST_DIALECTS.set("typescript");
    let mut config = config("");
    config.cgroup = Some(cgroup);
    config.confinement.address_space_bytes = 16 * GIB;
    config.run_bounds.memory = 8 * GIB;
    config.run_bounds.charge_limit = u64::MAX;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document(
        r#"
main {
  let big = text.repeat("x", 600000000)
  return 0
}"#,
    );
    let mut worker = checkout(&pool);
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.bounds.memory = 8 * GIB;
    start.bounds.charge_limit = u64::MAX;
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("start");
    let Err(lost) = worker.run(SLICE, false) else {
        panic!("the killed worker answers nothing")
    };
    assert_eq!(
        lost,
        PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
            limit: WorkerLimit::Memory {
                bound_bytes: CEILING
            }
        })
    );
    assert!(!lost.is_host_verdict(), "the run's own end: {lost:?}");
}

/// FIG-5876: a heap-profiled worker serves under the same confinement as
/// any other, and still writes its DHAT profile and receipt when its first
/// clean reset ends the window: the flush acknowledges the reset instead of
/// killing the worker, which then serves on.
#[cfg(feature = "dhat-heap")]
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the law hands the worker a fresh profile directory and reads what it wrote"
)]
fn a_heap_profiled_worker_writes_its_profile_under_confinement_across_a_reset() {
    let directory = std::env::temp_dir().join(format!("lash-heap-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let mut entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker"));
    entry.args = vec![
        "--heap-profile-dir".into(),
        directory.to_str().expect("utf-8 directory").into(),
    ];
    let mut config = PoolConfig::standard(entry);
    config.max_workers = 1;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document("main { return num.add(1, 2) }");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let End::Finished(finished) = ended(worker.run(SLICE, false).expect("run")) else {
        panic!("the run finishes")
    };
    assert_eq!(finished.result, int(3));
    worker
        .release()
        .expect("the reset flushes the profile and is acknowledged");
    let profile = directory.join(format!("vm-worker-{pid}.dhat.json"));
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile).expect("the profile")).expect("json");
    assert_eq!(written["mode"], "rust-heap", "{profile:?}");
    assert!(
        written["pps"]
            .as_array()
            .is_some_and(|sites| !sites.is_empty())
    );
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(profile.with_extension("receipt.json")).expect("the receipt"),
    )
    .expect("json");
    assert_eq!(receipt["completed"], true);
    let next = checkout(&pool);
    assert_eq!(next.pid(), Some(pid), "the profiled worker serves on");
    next.release().expect("a second reset after the window");
    let _ = std::fs::remove_dir_all(&directory);
}

/// FIG-6007: silence during the Ready handshake reports the actual wait,
/// just as silence during guest execution does. It is not an instant exit.
fn a_worker_silent_during_bootstrap_reports_the_time_it_was_silent() {
    let mut config = config("bootstrap_wait");
    config.protocol.no_response_watchdog = std::time::Duration::from_millis(20);
    let error = WorkerPool::new(config).err().expect("the watchdog fires");
    let PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive { silent_ms }) = error
    else {
        panic!("a typed silence verdict: {error:?}")
    };
    assert!(
        silent_ms >= 20,
        "the handshake waited, but reports {silent_ms} ms"
    );
}

/// The kernel worker's live protocol is admitted before any guest request.
/// Synthetic-next keeps the same refusal as the plain parent.
fn workers_refuse_another_protocol_before_guest_admission() {
    for mode in ["protocol_below", "protocol_above", "opposite_generation"] {
        let error = WorkerPool::new(config(mode))
            .err()
            .expect("incompatible worker");
        let PoolError::ProtocolVersion(refusal) = error else {
            panic!("typed protocol refusal: {error:?}");
        };
        assert_eq!(refusal.parent_version, WORKER_PROTOCOL_VERSION);
        assert_eq!(
            refusal.minimum_supported_version,
            MIN_SUPPORTED_WORKER_PROTOCOL_VERSION
        );
        assert!(
            refusal.worker_version < MIN_SUPPORTED_WORKER_PROTOCOL_VERSION
                || refusal.worker_version > WORKER_PROTOCOL_VERSION
        );
    }
}

/// FIG-6008: allocation tracing can put Ready beyond a serving worker's
/// silence window. Only profiled builds admit that longer bootstrap; the
/// configured live silence window remains the parent's working policy.
#[test]
fn bootstrap_uses_the_builds_allowance_without_extending_live_silence() {
    HOST_DIALECTS.set("typescript,python");
    let mut config = config("bootstrap_delayed");
    let silence = std::time::Duration::from_secs(1);
    config.protocol.no_response_watchdog = silence;
    let result = WorkerPool::new(config);
    #[cfg(feature = "dhat-heap")]
    {
        let pool = result.expect("Ready fits the profiled build's startup allowance");
        assert_eq!(pool.config().protocol.no_response_watchdog, silence);
    }
    #[cfg(not(feature = "dhat-heap"))]
    {
        let error = result
            .err()
            .expect("default builds keep one silence window");
        let PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive { silent_ms }) =
            error
        else {
            panic!("a typed startup silence verdict: {error:?}");
        };
        assert!(silent_ms >= 1_000, "the default startup watchdog waited");
    }
}

/// ADR 0123: authored workflow import uses the worker, including source limits
/// and front-end refusals. No parent lowering alternative is available.
#[test]
fn authored_workflow_import_is_worker_owned_and_preserves_source_policy() {
    HOST_DIALECTS.set("typescript");
    let workers = service::Service::new(config("normal")).with_worker_receipts();
    let embedding = lash_vm_worker::built_dialects(&WorkerTuning::standard()).expect("embedding");
    let effects = Default::default();
    let roots = Default::default();
    let controls = Default::default();
    let environment = lash_kernel_dialect::Environment {
        library: embedding.library(),
        effects: &effects,
        tool_roots: &roots,
        controls: &controls,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let typescript = lash_kernel_doc::SourceLanguage::TypeScript;
    let limits = |max_bytes| {
        Some(lash_kernel_dialect::workflow::WorkflowSourceLimits {
            max_bytes,
            max_nesting: 28,
        })
    };
    runtime.block_on(async {
        let source = "export async function digest(inbox: string) { return inbox; }";
        let (document, _) = workers
            .import_workflow(typescript, source, &environment, limits(1024))
            .await
            .expect("workflow import");
        assert!(document.entries.contains_key(&Name::new("digest")));
        let refused = workers
            .import_workflow(typescript, source, &environment, limits(1))
            .await;
        assert!(matches!(
            refused,
            Err(service::SourceServiceError::Dialect(_))
        ));
        let malformed = workers
            .import_workflow(
                typescript,
                "export const broken = ;",
                &environment,
                limits(1024),
            )
            .await;
        let Err(service::SourceServiceError::Dialect(refusal)) = malformed else {
            panic!("syntax is refused");
        };
        assert!(refusal.span.is_some());
    });
    let receipts = workers.worker_receipts();
    assert_eq!(receipts.len(), 3);
    assert!(receipts.iter().all(
        |receipt| receipt.path == service::WorkerPath::ImportWorkflow
            && receipt.pid != std::process::id()
    ));
}
