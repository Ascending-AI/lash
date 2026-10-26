use super::*;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::stage::StageLeaseId;
use lash_durable::{
    DurableError,
    domain::pages::{RootLease, StoredPage},
};
use lash_protocol_types::engine_fault::PayloadKind;
use lash_vm_broker::kernel::pages::PageExchange as _;
use lash_vm_broker::kernel::{
    MachinePark,
    pages::{ActivationPages, CapabilityKey, LeasedReads, PageLimits, SharedSpool, SpoolRoot},
};
use lash_vm_client::wire::{self, EndWire, ParkWire, StartWire};
use lash_vm_client::*;
use lash_vm_protocol::*;
use std::time::Duration;

struct Unread;
#[async_trait::async_trait]
impl LeasedReads for Unread {
    async fn read_pages(
        &self,
        _: &RootLease,
        _: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        unreachable!("no committed pages")
    }
    async fn renew_root_lease(&self, _: &RootLease) -> Result<RootLease, DurableError> {
        unreachable!("no committed lease")
    }
}

pub(super) fn run(
    program: &lash_kernel_vm::Program,
    file: &str,
    out: &std::path::Path,
    active: bool,
    stack: bool,
    bounds: Bounds,
    cells: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entry = WorkerEntry::helper(std::env::current_exe()?);
    entry
        .args
        .extend(["--cell-memory-dir".into(), out.display().to_string()]);
    if !active {
        entry.args.push("--cell-memory-off".into());
    }
    if stack {
        entry.args.push("--cell-stack".into());
    }
    let mut config = PoolConfig::standard(entry);
    config.max_workers = 1;
    // Diagnostic windows can be much slower than serving work. Both paired
    // modes use identical host limits; guest metering remains enabled.
    config.protocol.no_response_watchdog = Duration::from_secs(1800);
    config.deadlines.compute = Duration::from_secs(1800);
    config.deadlines.cumulative_cpu = Duration::from_secs(3600);
    config.deadlines.execution_wall = Duration::from_secs(3600);
    config.deadlines.serialization = Duration::from_secs(1800);
    config.run_bounds.memory = bounds.memory;
    config.run_bounds.charge_limit = bounds.charge_limit;
    config.run_bounds.call_depth = bounds.call_depth;
    config.tuning.slice = 1_000_000_000;
    config.tuning.pages.dirty_bytes = 512 << 20;
    config.confinement.address_space_bytes = 8 << 30;
    let run_bounds = config.run_bounds;
    let protocol = config.protocol;
    let pool = WorkerPool::new(config)?;
    let directory = tempfile::tempdir()?;
    let spools = SpoolRoot::open(directory.path(), 2 << 30)?;
    let spool = SharedSpool::new(spools.spool(StageLeaseId(1))?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let functions = program
        .functions
        .iter()
        .copied()
        .filter(|id| {
            program
                .documents
                .values()
                .any(|doc| doc.functions.contains_key(id))
        })
        .collect::<BTreeSet<_>>();
    let stored = program
        .documents
        .values()
        .flat_map(|doc| doc.functions.iter())
        .map(|(id, f)| (*id, Arc::new(f.clone())))
        .collect();
    let closure = lash_vm_broker::kernel::closure::derive(
        functions.clone(),
        program.library.registry(),
        &stored,
        program.helpers(),
        &program.document.library,
    )?
    .reference();
    let mut page_service = ActivationPages::new(
        Arc::new(Unread),
        None,
        Duration::from_secs(20),
        CapabilityKey::fresh(),
        1,
        Box::new(spool),
        Arc::new(lash_core_execution::runtime::SystemClock),
        Duration::from_secs(3600),
        PageLimits::within(protocol.decode),
    )
    .with_references([closure, NodeId([5; 32])]);
    let mut previous_pid = None;
    // Execute twice in the same process: the second window proves reset does
    // not permanently disable the collector. Each cell gets its own file.
    for ordinal in 1..=cells {
        let mut worker = pool.checkout(
            4 * 1024 * 1024 + FRAME_HEADER_BYTES,
            OwnerEpoch(1),
            FrameEpoch(ordinal),
            ExecutionBudget::default(),
        )?;
        let pid = worker.pid();
        if previous_pid.is_some() && pid != previous_pid {
            return Err("worker was replaced after reset".into());
        }
        previous_pid = pid;
        let start = Start {
            link: program.link,
            instances: wire::encode(
                PayloadKind::Start,
                &program.instances.values().collect::<Vec<_>>(),
            )?,
            documents: program
                .documents
                .iter()
                .map(|(id, doc)| message::StoredDocument::Supplied {
                    document: *id,
                    body: EncodedPayload(doc.canonical_bytes()),
                })
                .collect(),
            functions: functions.clone(),
            owner: VmOwner::new("cell-memory"),
            dialect: if file.ends_with(".py") {
                "python"
            } else {
                "typescript"
            }
            .into(),
            document: message::StoredDocument::supplied(EncodedPayload(
                program.document.canonical_bytes(),
            )),
            definitions: Vec::new(),
            from: StartFrom::Fresh(wire::encode(
                PayloadKind::Start,
                &StartWire::of(
                    Target::Main,
                    Vec::new(),
                    None,
                    &pages::modes(program),
                    Vec::new(),
                ),
            )?),
            bounds: run_bounds,
            binding: [5; 32],
            save: None,
            warm: Vec::new(),
            observe_sites: false,
            observe_counts: false,
            observe_path: false,
            memory_limiter: false,
            charge_limiter: false,
        };
        worker.start(
            start,
            &program.document.identity().to_string(),
            ExecutionClass::Cell,
        )?;
        let mut result = Datum::Null;
        loop {
            let mut step = worker.run(1_000_000_000, false)?;
            while let RunStep::Pages { id, request } = step {
                step = worker.page_reply(id, runtime.block_on(page_service.serve(request)))?;
            }
            match &step {
                RunStep::Parked {
                    seal: Some(batch), ..
                }
                | RunStep::Ended {
                    seal: Some(batch), ..
                } => {
                    worker.grant(runtime.block_on(page_service.admit(batch.clone()))?);
                }
                _ => {}
            }
            match step {
                RunStep::Slice { .. } => {}
                RunStep::Parked { park, .. } => {
                    let park: ParkWire = wire::decode(PayloadKind::Park, &park)?;
                    for request in MachinePark::try_from(park)?.requests {
                        let lash_kernel_vm::Request::Effect(request) = request;
                        let name = linked::operation(&request.use_);
                        let argument = request.args.into_iter().next().unwrap_or(Datum::Null);
                        if name.as_str() == "perf.finish" {
                            result = argument.clone();
                        }
                        let outcome = gate_six::answer(name.as_str(), argument);
                        worker.deliver(
                            request.wait,
                            wire::encode(PayloadKind::Outcome, &wire::OutcomeWire::from(outcome))?,
                        )?;
                    }
                }
                RunStep::Ended { end, meters, .. } => {
                    let end: EndWire = wire::decode(PayloadKind::End, &end)?;
                    if !matches!(end, EndWire::Finished { .. }) {
                        return Err(format!("worker cell failed: {end:?}").into());
                    }
                    println!(
                        "{}",
                        json!({"cell":ordinal,"process_id":pid,"result":format!("{result:?}"),"charged":meters.charged,"observer":active,"worker":true})
                    );
                    break;
                }
                other => return Err(format!("unexpected worker response: {other:?}").into()),
            }
        }
        worker.release()?;
    }
    Ok(())
}
