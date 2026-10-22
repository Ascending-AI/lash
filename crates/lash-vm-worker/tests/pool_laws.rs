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
use lash_kernel_doc::{Datum, Float, Handle, Integer, parse_document};
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
use lash_vocabulary::names::Name;
use lash_vocabulary::sites::EffectId;

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
    a_start_past_the_pools_ceiling_is_refused_and_one_at_it_runs_to_its_own_limit,
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
    let document = parsed(body);
    (
        document.identity().to_string(),
        EncodedPayload(document.canonical_bytes()),
    )
}

/// `body` as a document against the registry every worker assembles.
fn parsed(body: &str) -> lash_kernel_doc::Document {
    let release = lash_vm_releases::helper_release().expect("the linked worker fixture is valid");
    let mut text = String::from(
        "numbers by_spelling\nkernel 1\nimport 0 operation host.echo(x?: Any) -> Any\n",
    );
    for (_, function) in release.functions.iter() {
        if ["num.add", "num.lt", "text.repeat"].contains(&function.name()) {
            text.push_str(&format!(
                "definition {}\n",
                serde_json::to_string(&lash_kernel_doc::print_definition(function.definition()))
                    .expect("the linked worker fixture is valid")
            ));
            if function.definition().has_native() {
                text.push_str(&format!("native @{}\n", function.definition().identity()));
            }
        }
    }
    text.push_str(body);
    parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}"))
}

/// Pin a fixture's final Imports against one immutable helper release.
fn linked(document: lash_kernel_doc::Document) -> lash_kernel_vm::Program {
    use lash_vocabulary::{Fill, ImportKind, Link, MemberKind, OperationRequirement};
    let release = lash_vm_releases::helper_release().expect("the linked worker fixture is valid");
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let fill = match &import.kind {
                ImportKind::Operation { signature } => Fill::Operation(OperationRequirement::new(
                    signature.clone(),
                    lash_vocabulary::ContractId::new("1"),
                )),
                _ => {
                    let package = release
                        .content
                        .packages
                        .values()
                        .find(|package| package.path == import.name.package)
                        .expect("helper package");
                    let MemberKind::Export { link, export } =
                        &package.members[&import.name.member].kind
                    else {
                        panic!("helper export")
                    };
                    let mut export = export.clone();
                    export.member = import.name.type_member.clone();
                    Fill::Export {
                        link: *link,
                        export,
                    }
                }
            };
            (*slot, fill)
        })
        .collect();
    let root = Link {
        document: document.identity(),
        fills,
    };
    let id = root.identity();
    let mut links = release.content.links.clone();
    links.insert(id, root);
    let mut documents: std::collections::BTreeMap<_, _> = release
        .content
        .documents
        .iter()
        .map(|(id, document)| (*id, Arc::new(document.clone())))
        .collect();
    documents.insert(document.identity(), Arc::new(document));
    let embedding = lash_vm_worker::built_embedding(&WorkerTuning::standard())
        .expect("the linked worker fixture is valid");
    lash_kernel_vm::Program::prepare(
        id,
        documents,
        &links,
        lash_kernel_doc::heap::PageId([0; 32]),
        embedding
            .prepared_library()
            .expect("the linked worker fixture is valid")
            .clone(),
    )
    .expect("the linked worker fixture is valid")
}
fn echo_namespace(signature: lash_vocabulary::Signature) -> lash_kernel_dialect::Namespaces {
    let offer = lash_vocabulary::OperationOffer {
        requirement: lash_vocabulary::OperationRequirement::new(
            signature,
            lash_vocabulary::ContractId::new("1"),
        ),
        intents: Default::default(),
        controls: Default::default(),
        mode: lash_vocabulary::OperationMode::Waited,
    };
    lash_kernel_dialect::Namespaces::new(
        [(
            lash_vocabulary::PackagePath::parse("host")
                .expect("the linked worker fixture is valid"),
            [(
                lash_vocabulary::Segment::new("echo").expect("the linked worker fixture is valid"),
                lash_kernel_dialect::NamespaceMember {
                    visibility: lash_vocabulary::Visibility::Public,
                    kind: lash_kernel_dialect::NamespaceKind::Operation(offer.into()),
                },
            )]
            .into(),
        )]
        .into(),
        Default::default(),
        &|_| false,
    )
    .expect("the linked worker fixture is valid")
}

fn entry_document(source: &str) -> (String, EncodedPayload) {
    let (definition, locals) = lash_kernel_doc::parse_definition_with_locals(source)
        .expect("the linked worker fixture is valid");
    let mut document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    document.main = None;
    document.imports = definition
        .body()
        .map_or_else(Default::default, |body| body.imports.clone());
    document.exports.insert(
        Name::new("go"),
        lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
            function: definition.identity(),
            signature: definition.signature.clone(),
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.extend(
        locals
            .into_iter()
            .chain([definition])
            .map(|f| (f.identity(), f)),
    );
    (
        document.identity().to_string(),
        EncodedPayload(document.canonical_bytes()),
    )
}

fn bounds() -> RunBounds {
    PoolConfig::standard(WorkerEntry::helper("unused")).run_bounds
}

fn fresh(document: &EncodedPayload, target: Target, args: Vec<Datum>) -> Start {
    let program = linked(
        lash_kernel_doc::IdentifiedDocument::decode_standard(&document.0)
            .expect("the linked worker fixture is valid")
            .into_document(),
    );
    Start {
        link: program.link,
        instances: wire::encode(
            PayloadKind::Start,
            &program.instances.values().collect::<Vec<_>>(),
        )
        .expect("the linked worker fixture is valid"),
        documents: program
            .documents
            .iter()
            .map(
                |(id, doc)| lash_vm_protocol::message::StoredDocument::Supplied {
                    document: *id,
                    body: EncodedPayload(doc.canonical_bytes()),
                },
            )
            .collect(),
        functions: program.functions.clone(),
        owner: owner(),
        dialect: DIALECT.into(),
        document: lash_vm_protocol::message::StoredDocument::supplied(document.clone()),
        definitions: Vec::new(),
        from: StartFrom::Fresh(
            wire::encode(
                PayloadKind::Start,
                &StartWire::of(target, args, None, &Default::default(), Vec::new()),
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
    let program = linked(
        lash_kernel_doc::IdentifiedDocument::decode_standard(&document.0)
            .expect("the linked worker fixture is valid")
            .into_document(),
    );
    Start {
        link: program.link,
        instances: wire::encode(
            PayloadKind::Start,
            &program.instances.values().collect::<Vec<_>>(),
        )
        .expect("the linked worker fixture is valid"),
        documents: program
            .documents
            .iter()
            .map(
                |(id, doc)| lash_vm_protocol::message::StoredDocument::Supplied {
                    document: *id,
                    body: EncodedPayload(doc.canonical_bytes()),
                },
            )
            .collect(),
        functions: program.functions.clone(),
        owner: owner(),
        dialect: dialect.into(),
        document: lash_vm_protocol::message::StoredDocument::supplied(document.clone()),
        definitions: Vec::new(),
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

/// The execution closure `document`'s roots pin, as a parent derives it.
fn closure(document: &EncodedPayload) -> NodeId {
    let document = lash_kernel_doc::IdentifiedDocument::decode_standard(&document.0)
        .expect("the document decodes");
    let program = linked(document.into_document());
    let stored = program
        .documents
        .values()
        .flat_map(|doc| doc.functions.iter())
        .map(|(id, f)| (*id, Arc::new(f.clone())))
        .collect();
    lash_vm_broker::kernel::closure::derive(
        program.functions.iter().copied(),
        program.library.registry(),
        &stored,
    )
    .expect("the parent holds the document's functions")
    .reference()
}

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
    /// The page service of runs of `documents`, each pinning its
    /// execution closure.
    fn new(documents: &[&EncodedPayload]) -> Self {
        let directory = tempfile::tempdir().expect("spool directory");
        let spools = SpoolRoot::open(directory.path(), 1 << 30).expect("spools");
        let spool = SharedSpool::new(spools.spool(StageLeaseId(1)).expect("spool"));
        let mut references: Vec<NodeId> =
            documents.iter().map(|document| closure(document)).collect();
        references.push(NodeId(BINDING));
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
        .with_references(references);
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
fn requested(step: RunStep, document: &EncodedPayload) -> (lash_vocabulary::sites::WaitId, Datum) {
    let (wait, argument, _, _) = admitted(step, document);
    (wait, argument)
}

/// The one effect the park requests, as the parent's admission freezes it,
/// and the checkpoint root the park sealed.
fn admitted(
    step: RunStep,
    document: &EncodedPayload,
) -> (
    lash_vocabulary::sites::WaitId,
    Datum,
    EncodedPayload,
    [u8; 32],
) {
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
    let document = lash_kernel_doc::IdentifiedDocument::decode_standard(&document.0)
        .expect("the fixture document decodes");
    let (slot, _) = document
        .imports
        .iter()
        .find(|(_, import)| {
            import.name.package.to_string() == "host" && import.name.member.as_str() == "echo"
        })
        .expect("the fixture imports host.echo");
    let expected = lash_vocabulary::OperationUse {
        link: linked(document.shared().as_ref().clone()).link,
        slot: *slot,
    };
    assert_eq!(
        request.use_, expected,
        "the park requests the document's host.echo use"
    );
    let admission = lash_vm_broker::kernel::state::admission(&lash_kernel_state::AdmittedWait {
        identity: request.identity.clone(),
        request: lash_kernel_state::AdmittedRequest::Effect {
            use_: request.use_,
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
    let got = perform 0(step) as Any
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
    let (wait, argument) = requested(parent.run(&mut first, SLICE, false).expect("run"), document);
    asked.push(argument.clone());
    first.deliver(wait, completed(argument)).expect("deliver");
    let (wait, argument, admission, root) =
        admitted(parent.run(&mut first, SLICE, false).expect("run"), document);
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
        let (wait, argument) = requested(step, document);
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
    let (identity, document) = document(TOOL_LOOP);
    let mut parent = Parent::new(&[&document]);
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
    let sent = || pool.measurements().counters.ipc_sent_messages;
    // Never run: it makes the document, and so each start, large.
    let padding = "p".repeat(30 * 1024);
    let (identity, document) = document(&format!(
        r#"
main {{
  if num.lt(1, 0) {{ let unused = "{padding}" }}
  let kept = perform 0("a") as Any
  let again = perform 0(kept) as Any
  return again
}}"#
    ));
    let large = Datum::Text("y".repeat(40 * 1024));
    let mut parent = Parent::new(&[&document]);

    let mut first = checkout(&pool);
    first
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, _, admission, root) = admitted(
        parent.run(&mut first, SLICE, false).expect("run"),
        &document,
    );
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
    let (_, argument) = requested(
        parent.run(&mut second, SLICE, false).expect("run"),
        &document,
    );
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
    let echo = lash_vocabulary::types::Signature {
        params: vec![lash_vocabulary::types::Param {
            name: Name::new("x"),
            ty: lash_vocabulary::types::Type::Any,
            optional: false,
        }],
        result: lash_vocabulary::types::Type::Any,
    };
    let request = Request::Lower {
        dialect: "typescript".into(),
        source:
            "let total = 0; for (const step of [1, 2, 3]) { total = total + await host.echo(step); }"
                .into(),
        namespaces: echo_namespace(echo),
        bindings: Default::default(),
        functions: Default::default(),
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
    let mut parent = Parent::new(&[&document]);
    let (end, asked) =
        killed_at_a_park_and_resumed(&pool, &mut parent, DIALECT, &identity, &document);
    assert_eq!(asked.len(), 3, "each call is asked once: {asked:?}");
    assert_eq!(
        parent.carried(&end, "total"),
        Some(Datum::Float(Float::new(6.0)))
    );
}

/// FIG-5990, FIG-6985: a run names the dialect its session or process
/// recorded at creation. Running its document needs no dialect, so the
/// worker of a host that did not name it runs a new run and resumes a
/// parked one all the same; a run that saves its functions, which it
/// writes as the dialect's values, is refused with a typed refusal before
/// the worker reads the document or runs anything.
#[test]
fn a_worker_runs_code_in_a_dialect_its_host_did_not_name_and_refuses_to_save_it() {
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
    let mut parent = Parent::new(&[&document]);
    let mut worker = checkout(&pool);
    worker
        .start(new_run(), &identity, ExecutionClass::Cell)
        .expect("start");
    let (_, _, admission, root) = admitted(
        parent.run(&mut worker, SLICE, false).expect("a Python run"),
        &document,
    );
    let root = parent.granted(root);
    worker.release().expect("release at the park");
    drop(pool);

    // A host that names TypeScript alone runs both, and refuses a save.
    HOST_DIALECTS.set("typescript");
    let pool = WorkerPool::new(config("")).expect("pool");
    let parked = resumed("python", &document, root, vec![admission]);
    for (start, from) in [(new_run(), "a new run"), (parked, "a parked run")] {
        let mut worker = checkout(&pool);
        worker
            .start(start, &identity, ExecutionClass::Cell)
            .and_then(|()| parent.run(&mut worker, SLICE, false))
            .unwrap_or_else(|error| panic!("{from} runs without Python: {error:?}"));
    }
    let saving = Start {
        save: Some(
            wire::encode(
                PayloadKind::Start,
                &wire::SaveWire {
                    saved_origins: Default::default(),
                    controls: std::collections::BTreeMap::new(),
                    written: std::collections::BTreeMap::new(),
                },
            )
            .expect("a save encodes"),
        ),
        ..new_run()
    };
    let mut worker = checkout(&pool);
    let refused = worker
        .start(saving, &identity, ExecutionClass::Cell)
        .and_then(|()| parent.run(&mut worker, SLICE, false))
        .expect_err("the worker installs no Python to save in");
    assert!(
        matches!(
            &refused,
            PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                refusal: RunRefusal::DialectNotInstalled { dialect }
            }) if dialect == "python"
        ),
        "{refused:?}"
    );
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
    let echo = lash_vocabulary::types::Signature {
        params: vec![lash_vocabulary::types::Param {
            name: Name::new("x"),
            ty: lash_vocabulary::types::Type::Any,
            optional: false,
        }],
        result: lash_vocabulary::types::Type::Any,
    };
    let request = Request::Lower {
        dialect: "python".into(),
        source: "total = 0\nfor step in [1, 2, 3]:\n    total = total + await host.echo(step)\n"
            .into(),
        namespaces: echo_namespace(echo),
        bindings: Default::default(),
        functions: Default::default(),
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
    let mut parent = Parent::new(&[&document]);
    let (end, asked) =
        killed_at_a_park_and_resumed(&pool, &mut parent, "python", &identity, &document);
    assert_eq!(asked.len(), 3, "each call is asked once");
    assert_eq!(parent.carried(&end, "total"), Some(int(6)));
}

/// A parked run names its document; a worker handed another document
/// refuses the state before any machine is built.
fn a_parked_run_is_refused_under_another_document() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document_a) = document(TOOL_LOOP);
    let (other, document_b) = document("main { let got = perform 0(1) as Any return got }");
    let mut parent = Parent::new(&[&document_a, &document_b]);
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(&document_a, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (_, _, admission, root) = admitted(
        parent.run(&mut worker, SLICE, false).expect("run"),
        &document_a,
    );
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
    let (identity, document) = entry_document(
        "function go(n: Any) -> Any\nkernel 1\nmetering statements\nimport 0 operation host.echo(x?: Any) -> Any\nbody { let same = fn(value) { return value } for i in [1, 2] { let v = apply same(i) } let got = perform 0(n) as Any return got }",
    );
    let mut worker = checkout(&pool);
    let mut start = fresh(&document, Target::Entry(Name::new("go")), vec![int(7)]);
    start.observe_sites = true;
    worker
        .start(start, &identity, ExecutionClass::Process)
        .expect("start");
    let mut parent = Parent::new(&[&document]);
    let (wait, argument) = requested(
        parent.run(&mut worker, SLICE, false).expect("run"),
        &document,
    );
    let entered: Vec<Vec<EffectId>> = worker
        .take_sites()
        .iter()
        .map(|payload| {
            wire::decode::<wire::Sites>(PayloadKind::Sites, payload)
                .expect("sites")
                .entered
                .into_iter()
                .map(|location| location.at)
                .collect::<Vec<_>>()
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
        lash_vocabulary::sites::testkit::visit(1),
        "the second call is the last entry"
    );
    assert_eq!(
        lash_vocabulary::sites::ObservationReader::counts(call)
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
    let (identity, document) = entry_document(
        "function go(h: Handle(\"table\")) -> Any\nkernel 1\nmetering statements\nbody { print \"before\" let row = read(h, \"row 7\") print row return row }",
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
    let mut parent = Parent::new(&[&document]);
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
    assert_eq!(kinds, [HostReadKind::Projection]);
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
    assert_eq!(finished.result, Datum::Text("seven".into()));
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

/// FIG-6827: the pool's run bounds are the ceiling it admits, never a
/// clamp. A start asking more charge or memory than the ceiling is refused
/// with the typed refusal naming the bound and both numbers, and nothing
/// runs; a start asking exactly the ceiling runs to its own limit.
fn a_start_past_the_pools_ceiling_is_refused_and_one_at_it_runs_to_its_own_limit() {
    let mut config = config("");
    config.run_bounds.charge_limit = 1_000;
    let memory_ceiling = config.run_bounds.memory;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document("main { while true { } }");
    for (bound, past) in [
        (RunBound::Charge, (2_000, memory_ceiling)),
        (RunBound::Memory, (1_000, memory_ceiling + 1)),
    ] {
        let mut worker = checkout(&pool);
        let mut start = fresh(&document, Target::Main, Vec::new());
        (start.bounds.charge_limit, start.bounds.memory) = past;
        let refused = worker
            .start(start, &identity, ExecutionClass::Cell)
            .expect_err("a start past the ceiling is refused");
        let (requested, ceiling) = match bound {
            RunBound::Charge => (2_000, 1_000),
            _ => (memory_ceiling + 1, memory_ceiling),
        };
        assert!(
            matches!(
                refused,
                PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                    refusal: RunRefusal::AboveCeiling { bound: refused, requested: asked, ceiling: held },
                }) if refused == bound && asked == requested && held == ceiling
            ),
            "{refused:?}"
        );
        worker.release().expect("reset");
    }
    let mut worker = checkout(&pool);
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.bounds.charge_limit = 1_000;
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("a start at the ceiling");
    let End::Error(RunError::Bound(exceeded)) = ended(worker.run(SLICE, false).expect("run"))
    else {
        panic!("the charge bound ends the run")
    };
    assert_eq!(exceeded.bound, Bound::Charge);
    assert_eq!(exceeded.limit, 1_000);
    worker.release().expect("reset");
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
    let charge = document("main { while true { } }");
    let memory = document(
        r#"
main {
  let big = text.repeat("x", 200000)
  return 0
}"#,
    );
    let mut parent = Parent::new(&[&charge.1, &memory.1]);
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
    let (identity, document) = document(
        r#"
main {
  let i = 0
  while num.lt(i, 200) {
    set i = num.add(i, 1)
  }
  let got = perform 0(i) as Any
  let j = 0
  while num.lt(j, 200) {
    set j = num.add(j, 1)
  }
  return num.add(got, j)
}"#,
    );
    let mut parent = Parent::new(&[&document]);
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
        let (wait, argument, admission, root) = admitted(step, &document);
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
    let (identity, document) = document(TOOL_LOOP);
    let mut parent = Parent::new(&[&document]);
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, argument) = requested(
        parent.run(&mut worker, SLICE, false).expect("run"),
        &document,
    );
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

/// A host that never answers the time: a run that reads its clock waits
/// on it with its worker checked out.
struct Stalled;

#[async_trait::async_trait]
impl RunHost for Stalled {
    async fn read(
        &self,
        _: &Handle,
        _: &Datum,
    ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, lash_vm_broker::ParentFault> {
        std::future::pending().await
    }

    async fn perform(
        &self,
        _: &lash_vocabulary::names::OperationUse,
        _: &[Datum],
        _: &lash_vocabulary::sites::PositionLabel,
        _: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, lash_vm_broker::ParentFault> {
        panic!("the run binds no read")
    }

    fn print(&self, _: Datum) -> Result<(), lash_vm_broker::ParentFault> {
        Ok(())
    }
}

/// FIG-6788: a host read the host has not answered by the pool's
/// host-read deadline ends the run with the typed host-read limit, well
/// inside the worker's own parent wait (86,400 s), and the waiting worker
/// is not kept.
#[test]
fn a_host_read_past_its_deadline_ends_the_run_with_its_typed_limit() {
    use lash_vm_broker::kernel::{DrivenMachine as _, KernelFailure, Machines as _};

    HOST_DIALECTS.set("typescript");
    let mut config = config("");
    config.deadlines.host_read = Duration::from_millis(200);
    let service = service::Service::new(config);
    let (_, bytes) = entry_document(
        "function go(h: Handle(\"table\")) -> Any\nkernel 1\nmetering statements\nbody { let got = read(h, \"row\") return got }",
    );
    let document = lash_kernel_doc::IdentifiedDocument::decode_standard(&bytes.0)
        .expect("the linked worker fixture is valid");
    let program = linked(document.shared().as_ref().clone());
    let run = bounds();
    let machines = RemoteMachines::new(
        service.clone(),
        owner(),
        OwnerEpoch(1),
        FrameEpoch(1),
        Arc::new(Stalled),
        &document,
        DIALECT,
        RunStart {
            link: program.link,
            instances: program.instances.clone(),
            documents: program
                .documents
                .iter()
                .map(|(id, document)| {
                    (
                        *id,
                        lash_kernel_doc::IdentifiedDocument::from_shared(Arc::clone(document)),
                    )
                })
                .collect(),
            target: Target::Entry(Name::new("go")),
            args: vec![Datum::Handle(Handle {
                kind: "table".into(),
                id: "t1".into(),
            })],
            session: None,
            modes: Default::default(),
            host: Vec::new(),
            save: None,
        },
        lash_kernel_vm::Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: run.charge_limit,
            memory: run.memory,
            call_depth: run.call_depth,
            live_tasks: run.live_tasks,
            requests_per_park: run.requests_per_park,
            join_members: run.join_members,
        },
        lash_vm_broker::kernel::closure::derive(
            document.functions.keys().copied(),
            program.library.registry(),
            &document
                .functions
                .iter()
                .map(|(id, f)| (*id, Arc::new(f.clone())))
                .collect(),
        )
        .expect("the parent holds the document's functions"),
    )
    .expect("the run's machines");
    let directory = tempfile::tempdir().expect("spool directory");
    let spools = SpoolRoot::open(directory.path(), 1 << 30).expect("spools");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("a SQLite memory store set");
            let store: Arc<dyn lash_durable::domain::pages::PageStore> =
                Arc::new(stores.durable_store());
            let pages = ActivationPages::new(
                store,
                None,
                Duration::from_secs(20),
                CapabilityKey::fresh(),
                1,
                Box::new(spools.spool(StageLeaseId(1)).expect("spool")),
                Arc::new(lash_core_execution::runtime::SystemClock),
                Duration::from_secs(600),
                PageLimits::within(service.config().protocol.decode),
            )
            .with_references([
                machines.closure().expect("the closure").reference(),
                NodeId(BINDING),
            ]);
            let mut machine = machines
                .start(NodeId(BINDING), Arc::new(tokio::sync::Mutex::new(pages)))
                .await
                .expect("the run starts");
            // A guard against a parent that waits out the worker's parent
            // wait: the law fails here rather than hanging.
            let ran = tokio::time::timeout(Duration::from_secs(60), machine.run(SLICE, false))
                .await
                .expect("the parent stops waiting at the host-read deadline");
            assert!(
                matches!(
                    ran,
                    Err(KernelFailure::Worker {
                        outcome: InfrastructureOutcome::WorkerLimitExceeded {
                            limit: WorkerLimit::HostRead { bound_ms: 200 },
                        },
                    })
                ),
                "{ran:?}"
            );
        });
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
    let environment = lash_kernel_dialect::Namespaces::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let typescript = lash_kernel_doc::SourceLanguage::TypeScript;
    let limits = |max_bytes| {
        Some(lash_kernel_dialect::workflow::WorkflowSourceLimits {
            max_bytes,
            max_nesting: 28,
            implicit_text_conversion: Default::default(),
        })
    };
    runtime.block_on(async {
        let source = "export async function digest(inbox: string) { return inbox; }";
        let (document, _, _) = workers
            .import_workflow(typescript, source, &environment, limits(1024))
            .await
            .expect("workflow import");
        assert!(document.exports.contains_key(&Name::new("digest")));
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

#[path = "../src/fixture/pool_package.rs"]
mod pool_package;

/// The parent's code-only package; the shipped worker is given no package list.
fn pool_library(source: &str) -> Arc<lash_vm_client::packages::WorkerLibrary> {
    let release = lash_vm_releases::helper_release().expect("the linked worker fixture is valid");
    let mut registry = release.functions.clone();
    let definition =
        lash_kernel_doc::parse_definition(source).expect("the linked worker fixture is valid");
    let id = registry
        .register(definition, None)
        .expect("the linked worker fixture is valid");
    Arc::new(
        lash_vm_client::packages::WorkerLibrary::new(
            &registry,
            release.resolver_roots().into_iter().chain([id]),
            &release.content.documents,
        )
        .expect("the linked worker fixture is valid"),
    )
}

fn pool_call(worker: &mut Checkout, definition: &str) -> (EndWire, Option<u64>) {
    let definition = lash_kernel_doc::parse_definition(definition).expect("definition");
    let _function = definition.identity();
    let name = definition.name.clone();
    let document = parse_document(&format!(
        "kernel 1\nnumbers by_spelling\ndefinition {}\nmain {{ let x = invoke {name}(7) return x }}", serde_json::to_string(&lash_kernel_doc::print_definition(&definition)).expect("the linked worker fixture is valid")
    )).expect("fixture document");
    let bytes = EncodedPayload(document.canonical_bytes());
    worker
        .start(
            fresh(&bytes, Target::Main, Vec::new()),
            &document.identity().to_string(),
            ExecutionClass::Cell,
        )
        .expect("start");
    let step = worker.run(SLICE, false).expect("run");
    let RunStep::Ended { end, meters, .. } = step else {
        panic!("fixture ends: {step:?}")
    };
    (
        wire::decode(PayloadKind::End, &end).expect("end"),
        meters.charged,
    )
}

/// K-LIB-002: the same shipped package function, run with a native by the
/// fixture executable and without one by the unmodified shipped worker,
/// answers with exactly the same value and charge.
#[test]
fn a_shipped_both_function_without_its_native_answers_by_its_body_with_the_same_value_and_charge() {
    HOST_DIALECTS.set("typescript");
    let packages = pool_library(pool_package::BOTH);
    let mut native = config("package_native");
    native.library = Some(packages.clone());
    let native = WorkerPool::new(native).expect("native-enabled worker");
    let mut worker = checkout(&native);
    let expected = pool_call(&mut worker, pool_package::BOTH);
    assert!(matches!(&expected.0, EndWire::Finished { result, .. } if *result == int(7)));
    worker.release().expect("reset native");

    let mut config =
        PoolConfig::standard(WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker")));
    config.library = Some(packages);
    let body = WorkerPool::new(config).expect("unmodified shipped worker");
    let mut worker = checkout(&body);
    assert_eq!(pool_call(&mut worker, pool_package::BOTH), expected);
    worker.release().expect("reset body");
}

/// K-LIB-006 over a shipped package: equal native arguments answer equally
/// under a warm worker and a fresh process, including the charge.
#[test]
fn a_shipped_packages_native_is_pure_across_warm_and_cold_workers() {
    let held = lash_vm_releases::helper_release().expect("helper release");
    let function = held
        .functions
        .named("num.abs")
        .next()
        .expect("a shipped native");
    let definition = lash_kernel_doc::print_definition(
        held.functions
            .get(&function)
            .expect("the native")
            .definition(),
    );
    let mut config =
        PoolConfig::standard(WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker")));
    config.max_workers = 1;
    config.library = Some(Arc::new(
        lash_vm_client::packages::WorkerLibrary::new(
            &held.functions,
            held.resolver_roots(),
            &held.content.documents,
        )
        .expect("the linked worker fixture is valid"),
    ));
    let pool = WorkerPool::new(config.clone()).expect("warm pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid();
    let expected = pool_call(&mut worker, &definition);
    assert!(matches!(&expected.0, EndWire::Finished { result, .. } if *result == int(7)));
    worker.release().expect("reset");
    let mut worker = checkout(&pool);
    assert_eq!(worker.pid(), pid);
    assert_eq!(pool_call(&mut worker, &definition), expected);
    worker.release().expect("reset warm");
    let cold = WorkerPool::new(config).expect("cold pool");
    let mut worker = checkout(&cold);
    assert_ne!(worker.pid(), pid);
    assert_eq!(pool_call(&mut worker, &definition), expected);
    worker.release().expect("reset cold");
}

/// POOL-FRAME: reset and process replacement preserve the pool-start
/// frame for the pool's life.
#[test]
fn a_workers_registry_is_the_pool_start_frame_unchanged_for_its_life() {
    HOST_DIALECTS.set("typescript");
    let later = pool_package::BOTH.replace("return x", "return 99");
    let held = pool_library(&later);
    let mut config =
        PoolConfig::standard(WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker")));
    config.library = Some(held);
    let pool = WorkerPool::new(config).expect("parent's own frame");
    let mut worker = checkout(&pool);
    let pid = worker.pid();
    let own = pool_call(&mut worker, &later);
    assert!(matches!(&own.0, EndWire::Finished { result, .. } if *result == int(99)));
    worker.release().expect("reset");
    let mut worker = checkout(&pool);
    assert_eq!(worker.pid(), pid);
    assert_eq!(pool_call(&mut worker, &later), own);
    // Dropping a checkout discards its worker. Its replacement gets the
    // original frame rather than rebuilding from the worker binary.
    drop(worker);
    let mut worker = checkout(&pool);
    assert_ne!(worker.pid(), pid);
    assert_eq!(pool_call(&mut worker, &later), own);
    worker.release().expect("reset replacement");
}

/// NativeMissing is a pool-start refusal, before the first guest checkout.
#[test]
fn a_frame_holding_a_native_only_function_the_shipped_worker_lacks_is_refused_at_pool_start() {
    use lash_protocol_types::engine_fault::WorkerLibraryRefusal;
    let source = pool_package::BOTH.replace("body { return x }\n", "");
    let definition = lash_kernel_doc::parse_definition(&source).expect("native-only definition");
    let mut config =
        PoolConfig::standard(WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker")));
    config.min_workers = 0;
    config.library = Some(pool_library(&source));
    let error = WorkerPool::new(config)
        .err()
        .expect("refused before checkout");
    let expected = WorkerLibraryRefusal::NativeMissing {
        function: definition.identity(),
        name: definition.name,
    };
    assert_eq!(error, PoolError::Library(expected.clone()));
    let runtime = error.into_runtime_error();
    let Some(lash_core_execution::RuntimeErrorCause::VmWorker { outcome }) = runtime.cause else {
        panic!("package refusal retains its worker cause");
    };
    let InfrastructureOutcome::ProtocolViolation {
        breach:
            lash_protocol_types::engine_fault::ProtocolBreach::Pool {
                fault: lash_protocol_types::engine_fault::PoolFault::Library { refusal },
            },
    } = *outcome
    else {
        panic!("package refusal stays typed at the runtime boundary");
    };
    assert_eq!(refusal, expected);
}
