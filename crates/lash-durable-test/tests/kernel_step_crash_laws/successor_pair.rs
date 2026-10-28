//! FIG-7152: linked succession publishes the record, environment and parked
//! machine together, retaining the calls and waits admitted before the carry.
//!
//! Setup is deliberately outside the cut census: every cell first witnesses
//! a real parked machine and an admitted timer call. Only then does it cut
//! each successor advance, inspect storage before another node can recover,
//! and cold-claim the result. A later repair cannot hide a mixed pair.

use super::{kernel_process, served, sim};
use lash::package::{LinkedStart, StartSelector};
use lash::process::{ProcessEnv, ProcessInput, ProcessSuccessor};
use lash_core_execution::runtime::actor::process::state_graph::read_engine_state;
use lash_core_execution::runtime::actor::round::{self, PinnedWaits};
use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder};
use lash_durable::domain::{ExecKey, OwnerKey, RootOwner, RunFacts};
use lash_durable::{ActorKey, ActorState, CommitLabel};
use lash_durable_test::{Fault, Matrix, Script, SimClock, SimNodes, SimNodesConfig};
use lash_kernel_doc::heap::{PageGrant, PageId, PageRef};
use lash_kernel_state::paged::Checkpoint;
use lash_vocabulary::identity::ProcessId;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[path = "../support/dialect.rs"]
mod dialect;

const SOURCE: &str = r#"kernel 1
numbers float
import 0 operation typescript.sleep(milliseconds: Int) -> Null
main {
  let saved = [7, "retained"]
  do perform 0(50000000) as Null
  return saved
}
"#;
const LIMITS: DecodeLimits = DecodeLimits::within_total(64 * 1024 * 1024);

struct Pair {
    payload: Value,
    env: ProcessEnv,
    start: LinkedStart,
}

async fn publish(core: &lash::LashCore, next: bool) -> Pair {
    // An unused definition changes the Document/Link without changing any
    // live site's meaning. The mapping must therefore carry old uses to new
    // Link-qualified uses rather than accidentally exercising identity only.
    let source = if next {
        let extra = lash_kernel_doc::parse_definition(
            "function carry.unused() -> Int\nkernel 1\ncharge 1\nbody { return 99 }",
        )
        .unwrap();
        format!(
            "{SOURCE}\ndefinition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(&extra)).unwrap()
        )
    } else {
        SOURCE.into()
    };
    let base = ProcessEnv::builder(lash::MaxToolCalls::new(if next { 17 } else { 16 }))
        .component(super::SERVICES, &())
        .build()
        .unwrap();
    let (payload, env) = kernel_process::publish(core, &source, &base).await;
    let input: lash_vm_runtime::KernelProcessInput =
        serde_json::from_value(payload.clone()).unwrap();
    let lash_vm_runtime::process::InlineRequest::Linked { start } = input.code else {
        panic!("the law uses linked starts")
    };
    assert_eq!(start.selector, StartSelector::Main);
    Pair {
        payload,
        env,
        start,
    }
}

struct Stored {
    payload: Value,
    env: ProcessEnv,
    state: Value,
    page: Checkpoint,
    facts: RunFacts,
    calls: Vec<lash_core::ToolCallId>,
    waits: PinnedWaits,
    driver: Value,
}

async fn stored(core: &lash::LashCore, process: &ProcessId) -> Stored {
    let record = core.processes().get(process).await.unwrap().unwrap();
    assert!(
        record.terminal().is_none(),
        "the parked process ended: {record:?}"
    );
    let ProcessInput::Engine {
        payload, launch, ..
    } = &record.input
    else {
        panic!("an engine process")
    };
    let env = lash_execution_contracts::process::load_process_env(
        core.backend().process_env_store().as_ref(),
        &launch.env,
    )
    .await
    .unwrap();
    let reads = core.backend().durable();
    let (root, _, state) = read_engine_state(reads.as_ref(), process, LIMITS)
        .await
        .unwrap()
        .unwrap();
    let graph = reads
        .hydrate(
            RootOwner::Exec(ExecKey::Process(process.clone())),
            Some(root.root),
            LIMITS,
        )
        .await
        .unwrap();
    let page = state.pages.expect("a real parked page");
    let answer = lash_vm_broker::kernel::pages::answer_page(
        PageRef {
            id: PageId(page.0),
            grant: PageGrant::NONE,
        },
        &graph.nodes.get(&page).unwrap().bytes,
        |_| Ok(PageGrant::NONE),
    )
    .unwrap();
    // Also decode the enclosing graph: a root whose advertised requirements
    // or child edges are corrupt cannot qualify as either complete pair.
    let _ = lash_core_execution::runtime::actor::process::state_graph::load_engine_state(
        &mut GraphDecoder::new(&graph, LIMITS),
        root.root,
        process,
    )
    .unwrap();
    let facts = reads
        .run_facts(&OwnerKey::Process(process.clone()))
        .await
        .unwrap();
    let waits = PinnedWaits::read(reads.as_ref(), &facts).await.unwrap();
    let fold = round::fold(&facts, &Default::default(), &waits).unwrap();
    let calls = fold
        .rounds()
        .flat_map(|round| round.members())
        .filter(|member| member.outcome().is_none())
        .map(|member| member.call().clone())
        .collect();
    Stored {
        payload: payload.clone(),
        env,
        state: serde_json::from_slice(&state.bytes).unwrap(),
        page: Checkpoint::of_answer(&answer).unwrap(),
        facts,
        calls,
        waits,
        driver: serde_json::from_str(
            &reads
                .process(process)
                .await
                .unwrap()
                .unwrap()
                .driver_json
                .unwrap(),
        )
        .unwrap(),
    }
}

fn assert_pair(value: &Stored, before: &Stored, old: &Pair, new: &Pair) -> bool {
    let next = value.payload == new.payload;
    let pair = if next { new } else { old };
    assert_eq!(
        value.payload, pair.payload,
        "the record is one complete start"
    );
    assert_eq!(
        value.state["payload"], pair.payload,
        "mixed successor pair: engine payload and record"
    );
    assert_eq!(
        value.page.header().link,
        pair.start.link,
        "mixed successor pair: parked page and record"
    );
    assert_eq!(
        value.env.execution().authored,
        pair.env.execution().authored,
        "mixed successor pair: environment and record"
    );
    assert_eq!(
        value.env.max_tool_calls(),
        pair.env.max_tool_calls(),
        "mixed successor pair: environment policy and record"
    );
    for (use_, binding) in &pair.env.execution().uses {
        assert_eq!(
            value.env.execution().uses.get(use_),
            Some(binding),
            "UseCorrespondence preserves the accepted binding at {use_:?}"
        );
    }
    assert_eq!(value.state["phase"], "parked");
    for field in ["admissions", "waits", "runs", "effects", "settled"] {
        assert_eq!(
            value.state[field], before.state[field],
            "carry changed {field}"
        );
    }
    assert_eq!(value.page.header().owner, before.page.header().owner);
    assert_eq!(value.page.header().charged, before.page.header().charged);
    assert_eq!(value.page.header().limits, before.page.header().limits);
    assert!(
        value.page.same_pending_waits(&before.page),
        "parked waits keep their identities"
    );
    assert_eq!(
        value.calls, before.calls,
        "admitted calls keep their identities"
    );
    assert_eq!(
        value.facts, before.facts,
        "carry does not re-admit or settle calls"
    );
    assert_eq!(
        value.waits, before.waits,
        "completion waits retain their owner, deadline and lifecycle"
    );
    if next {
        assert!(
            value.driver.get("successor").is_none_or(Value::is_null),
            "the new pair commits removal of its pending successor request"
        );
    }
    next
}

async fn cell(tier: dialect::Dialect, cut: Option<(usize, Fault)>) -> usize {
    let clock = SimClock::new();
    let keep = Mutex::default();
    let url = if tier == dialect::Dialect::Postgres {
        Some(dialect::postgres_url().expect("the PostgreSQL law requires its service"))
    } else {
        None
    };
    let (stores, database) = dialect::open(tier, url.as_deref(), Arc::clone(&clock), &keep).await;
    let backend = served::configured_backend(stores, sim::settings(), Vec::new());
    let core =
        lash::LashCore::rlm_builder(backend.clone(), served::rlm(&backend, sim::workers(&clock)))
            .serve_sessions(false)
            .process_component(Arc::new(super::services()), super::SERVICES)
            .commit_budget(lash::CommitBudget {
                nodes: lash::CommitBudgetLimit::bounded(4096),
                ..lash::CommitBudget::default()
            })
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("successor-pair"),
                lash::persistence::LeaseIncarnationId::new("boot"),
            ))
            .unwrap();
    let (descriptor, activation) =
        lash::testing::node_activation(&core, Arc::new(lash_durable::NoProbe)).unwrap();
    let script = Script::new();
    let nodes = SimNodes::new(
        database,
        clock,
        script,
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: descriptor.formats().decodes(),
            serves: lash::testing::node_serves(&core),
            max_active: 4,
        },
        activation,
    );
    let script = nodes.script();
    let old = publish(&core, false).await;
    let new = publish(&core, true).await;
    assert_ne!(
        old.start.link, new.start.link,
        "nonidentity UseCorrespondence"
    );
    let pin = lash_core::HostArtifactPin::mint();
    let env = core
        .host_artifacts()
        .publish_process_env(&pin, &old.env)
        .await
        .unwrap();
    let old_env = env.clone();
    let settings = core
        .processes()
        .engine_defaults(lash_vm_runtime::LASH_VM_ENGINE_KIND, &old.payload)
        .unwrap();
    let process = core
        .processes()
        .start(
            lash::process::ProcessStartRequest::engine(
                lash_vm_runtime::LASH_VM_ENGINE_KIND,
                old.payload.clone(),
                lash::process::EngineLaunch::new(env, settings),
                lash::process::ProcessOriginator::host(),
                lash::process::LifetimeDecision::Detached,
            ),
            core.effect_host(),
        )
        .await
        .unwrap()
        .process_id;
    let actor = ActorKey::process(process.as_str()).unwrap();
    nodes.start("a");
    nodes.quiesce().await;
    while nodes.database().actor(&actor).await.unwrap().unwrap().state != ActorState::Waiting {
        assert!(
            nodes.step().await.is_some(),
            "the predecessor reaches its park"
        );
        assert!(
            nodes.clock().logical_ms() < 60_000,
            "setup must park before its timer fires"
        );
    }
    let mut no_op = ProcessSuccessor::new(old.payload.clone(), old_env.clone());
    no_op.request_id = Some("already-current-law".into());
    assert_eq!(
        core.processes().succeed(&process, no_op).await.unwrap(),
        lash::process::ProcessSuccession::Requested
    );
    nodes.quiesce().await;
    let before = stored(&core, &process).await;
    assert_eq!(before.state["phase"], "parked");
    assert_eq!(before.state["waits"].as_object().unwrap().len(), 1);
    assert_eq!(before.state["admissions"].as_object().unwrap().len(), 1);
    assert_eq!(before.page.header().pending_waits, 1);
    assert_eq!(
        before.calls.len(),
        1,
        "a real call was admitted before succession"
    );
    assert_ne!(
        before.waits,
        PinnedWaits::default(),
        "its completion wait is stored"
    );
    let advances = || {
        script
            .trace()
            .iter()
            .filter(|write| write.point.label == CommitLabel::PROCESS_ADVANCE)
            .count()
    };
    let offset = advances();
    if let Some((nth, fault)) = cut {
        script.cut(CommitLabel::PROCESS_ADVANCE, offset + nth, fault);
    }
    let env = core
        .host_artifacts()
        .publish_process_env(&pin, &new.env)
        .await
        .unwrap();
    let mut successor = ProcessSuccessor::new(new.payload.clone(), env);
    successor.request_id = Some("successor-law".into());
    assert_eq!(
        core.processes().succeed(&process, successor).await.unwrap(),
        lash::process::ProcessSuccession::Requested
    );
    let mut pending_retry = ProcessSuccessor::new(old.payload.clone(), old_env.clone());
    pending_retry.request_id = Some("successor-law".into());
    assert_eq!(
        core.processes()
            .succeed(&process, pending_retry)
            .await
            .unwrap(),
        lash::process::ProcessSuccession::Requested,
        "a queued retry keeps the first request's target"
    );
    nodes.quiesce().await;
    // There is only one node until this inspection. In particular, an
    // abort cannot be repaired by a peer before the old-or-whole-new oracle.
    assert_pair(&stored(&core, &process).await, &before, &old, &new);
    if let Some((nth, fault)) = cut {
        while script.cuts().is_empty() {
            assert!(nodes.step().await.is_some());
            assert_pair(&stored(&core, &process).await, &before, &old, &new);
            assert!(
                nodes.clock().logical_ms() < 60_000,
                "the requested cut was reached"
            );
        }
        let cuts = script.cuts();
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].point.nth, offset + nth);
        assert_eq!(cuts[0].fault, fault);
    }
    nodes.start("b");
    nodes.quiesce().await;
    loop {
        let next = assert_pair(&stored(&core, &process).await, &before, &old, &new);
        if next
            && nodes.database().actor(&actor).await.unwrap().unwrap().state == ActorState::Waiting
        {
            break;
        }
        assert!(nodes.step().await.is_some(), "cold recovery makes progress");
        assert!(
            nodes.clock().logical_ms() < 60_000,
            "recovery finishes before any admitted wait expires"
        );
    }
    let env = core
        .host_artifacts()
        .publish_process_env(&pin, &old.env)
        .await
        .unwrap();
    let mut duplicate = ProcessSuccessor::new(old.payload.clone(), env);
    duplicate.request_id = Some("successor-law".into());
    assert_eq!(
        core.processes().succeed(&process, duplicate).await.unwrap(),
        lash::process::ProcessSuccession::AlreadyResolved
    );
    assert!(
        assert_pair(&stored(&core, &process).await, &before, &old, &new),
        "a repeated request id must not switch back to an earlier selection"
    );
    let mut no_op_retry = ProcessSuccessor::new(old.payload.clone(), old_env);
    no_op_retry.request_id = Some("already-current-law".into());
    assert_eq!(
        core.processes()
            .succeed(&process, no_op_retry)
            .await
            .unwrap(),
        lash::process::ProcessSuccession::AlreadyResolved
    );
    assert!(assert_pair(
        &stored(&core, &process).await,
        &before,
        &old,
        &new
    ));
    let count = advances() - offset;
    nodes.kill("a");
    nodes.kill("b");
    nodes.quiesce().await;
    assert!(script.disarm_unfired().is_empty());
    count
}

async fn law(tier: dialect::Dialect) {
    let advances = Box::pin(cell(tier, None)).await;
    assert!(
        advances >= 2,
        "census includes recording and applying the successor"
    );
    let faults = [
        Fault::Abort,
        Fault::CommitThenAbort,
        Fault::FailBefore,
        Fault::AckHidden,
    ];
    for nth in 1..=advances {
        for fault in faults {
            eprintln!("successor pair {tier:?}: advance {nth}/{advances}, {fault}");
            Box::pin(cell(tier, Some((nth, fault)))).await;
        }
    }
    eprintln!(
        "successor pair {tier:?}: {} cut cells, {advances} advances, 1 uncut census",
        advances * faults.len()
    );
}

#[tokio::test]
async fn a_successor_pair_cut_at_every_advance_leaves_the_old_pair_or_the_whole_new_one() {
    law(dialect::Dialect::SqliteMemory).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_successor_pair_cut_at_every_advance_leaves_the_old_pair_or_the_whole_new_one_postgres() {
    let _ = lash_postgres_store::testing::required_database_url();
    law(dialect::Dialect::Postgres).await;
}
