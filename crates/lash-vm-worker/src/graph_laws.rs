//! The worker's graph state (FIG-5887 §3, §9, §11): PC-WHOLE and the leaf
//! placement of large values through the real staging, and the release,
//! native and admission refusals of a resume, against a TypeScript
//! embedding.
#![expect(
    clippy::unwrap_used,
    reason = "law helpers fail on broken fixture assumptions"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::graph::{GraphStateFailure, ReleaseGraph, admission, export_parked, import_parked};
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
use lash_core_store::checkpoint_graph::{DecodeLimits, NodeKind, check_envelope};
use lash_kernel_doc::{
    Datum, Document, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp, parse_document,
};
use lash_kernel_state::{AdmittedRequest, AdmittedWait};
use lash_kernel_vm::{
    Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, Start, Step, Target,
};
use lash_vm_client::WorkerTuning;
use lash_vm_protocol::{KernelRestoreRefusal, ParkedGraph, SealedGraphState, VmOwner};
use lash_vm_releases::HeldManifests;

use crate::embedding::Embedding;
use crate::embedding::tests::typescript_only as embedding;

/// The dialect the laws' runs pin the release of.
const DIALECT: &str = lash_vm_releases::TYPESCRIPT;

struct Quiet;

impl Host for Quiet {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Ok(Datum::Null)
    }

    fn print(&mut self, _: &Datum) {}

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

const BOUNDS: Bounds = Bounds {
    charge: 50_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

/// A large text held across two waits and echoed back, and a list of small
/// values too large for one node.
const LARGE: &str = r#"
main {
  let big = text.repeat("x", 70000)
  let xs = []
  let i = 0
  while num.lt(i, 20000) {
    set xs[i] = i
    set i = num.add(i, 1)
  }
  let a = perform echo(big) as Any
  set xs[0] = a
  let b = perform echo("small") as Any
  return (list.len(xs), b)
}
"#;

fn document(embedding: &Embedding, body: &str) -> Arc<Document> {
    let mut text = String::from("numbers by_spelling\nkernel 1\neffect echo(x?: Any) -> Any\n");
    for (name, id) in embedding.library().iter() {
        if ["num.add", "num.lt", "text.repeat", "list.len"].contains(&name.to_string().as_str()) {
            text.push_str(&format!("use {name} = @{id}\n"));
        }
    }
    text.push_str(body);
    Arc::new(parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}")))
}

fn program(embedding: &Embedding, document: &Arc<Document>) -> Program {
    Program {
        document: Arc::clone(document),
        library: embedding
            .prepared_for(document.manifest.kernel)
            .unwrap()
            .clone(),
    }
}

fn binding() -> OwnerBinding {
    OwnerBinding::Cell {
        session: "session-A".into(),
        turn: "turn-1".into(),
        cell: "cell-1".into(),
    }
}

/// Runs to the next park and answers nothing; returns the requests it
/// handed out.
fn park(machine: &mut KernelMachine) -> Vec<Request> {
    match machine.run(&mut Quiet, u64::MAX).unwrap() {
        Step::Parked(park) => park.requests,
        other => panic!("the run parks: {other:?}"),
    }
}

fn echo(machine: &mut KernelMachine, request: &Request) {
    let Request::Effect(effect) = request else {
        panic!("an effect");
    };
    machine
        .deliver(effect.wait, Outcome::Completed(effect.args[0].clone()))
        .unwrap();
}

fn admitted(requests: &[Request]) -> Vec<lash_vm_protocol::EncodedPayload> {
    requests
        .iter()
        .map(|request| {
            let Request::Effect(effect) = request else {
                panic!("an effect");
            };
            admission(&AdmittedWait {
                identity: effect.identity.clone(),
                request: AdmittedRequest::Effect {
                    effect: effect.effect.clone(),
                    args: effect.args.clone(),
                },
            })
        })
        .collect()
}

fn export(
    embedding: &Embedding,
    machine: &mut KernelMachine,
    document: &Document,
) -> SealedGraphState {
    export_parked(
        machine,
        VmOwner::new("session-A"),
        &binding(),
        embedding
            .release_manifests()
            .unwrap()
            .manifests
            .current(DIALECT)
            .unwrap(),
        &document.identity().unwrap().to_string(),
        64 << 20,
    )
    .unwrap()
}

fn kinds(state: &SealedGraphState) -> BTreeMap<NodeId, NodeKind> {
    state
        .nodes()
        .iter()
        .map(|node| {
            let id = NodeId(node.id);
            let envelope =
                check_envelope(&node.bytes, id, &DecodeLimits::within_total(u64::MAX)).unwrap();
            (id, envelope.kind)
        })
        .collect()
}

fn limits() -> DecodeLimits {
    DecodeLimits::within_total(1 << 30)
}

/// PC-WHOLE and the large-value placement, through the real staging: a
/// structure too large for one node streams as complete blocks, a text past
/// the inline bound is shared leaves that the next park's whole state
/// stages identically, the state's own structure holds no copy of it, and a
/// machine resumed cold from the second park in another program finishes
/// as the run that never parked.
#[test]
fn a_whole_state_streams_large_structure_and_shares_large_text() {
    let embedding = embedding(&WorkerTuning::standard()).unwrap();
    let document = document(&embedding, LARGE);
    let straight = {
        let mut machine =
            KernelMachine::start(program(&embedding, &document), BOUNDS, start()).unwrap();
        let first = park(&mut machine);
        echo(&mut machine, &first[0]);
        let second = park(&mut machine);
        echo(&mut machine, &second[0]);
        machine.run(&mut Quiet, u64::MAX).unwrap()
    };

    let mut machine =
        KernelMachine::start(program(&embedding, &document), BOUNDS, start()).unwrap();
    let first = park(&mut machine);
    let one = export(&embedding, &mut machine, &document);
    let one_kinds = kinds(&one);
    let blocks = one_kinds
        .values()
        .filter(|kind| **kind == NodeKind::STRUCTURE_BLOCK)
        .count();
    assert!(blocks >= 2, "the list streams as blocks, {blocks}");
    let leaves: BTreeSet<NodeId> = one_kinds
        .iter()
        .filter(|(_, kind)| **kind == NodeKind::TEXT_LEAF)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(leaves.len(), 2, "70000 bytes take two leaves");
    let run = [b'x'; 257];
    assert!(
        one.nodes()
            .iter()
            .filter(|node| !leaves.contains(&NodeId(node.id)))
            .all(|node| !node.bytes.windows(run.len()).any(|window| window == run)),
        "no copy of the text outside its leaves"
    );

    echo(&mut machine, &first[0]);
    let second = park(&mut machine);
    let two = export(&embedding, &mut machine, &document);
    let two_kinds = kinds(&two);
    // The held text, the delivered outcome and the request all name it: it
    // is still exactly the same two leaves.
    let two_leaves: BTreeSet<NodeId> = two_kinds
        .iter()
        .filter(|(_, kind)| **kind == NodeKind::TEXT_LEAF)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(two_leaves, leaves);

    let mut requests = first;
    requests.extend(second.iter().cloned());
    let parked = ParkedGraph {
        state: two,
        admissions: admitted(&requests),
    };
    let releases = embedding.release_manifests().unwrap();
    let registry = Arc::clone(program(&embedding, &document).library.registry());
    let (mut resumed, pinned) = import_parked(
        program(&embedding, &document),
        BOUNDS,
        &parked,
        releases,
        &registry,
        limits(),
    )
    .unwrap();
    assert_eq!(Some(pinned), releases.manifests.current(DIALECT));
    echo(&mut resumed, &second[0]);
    assert_eq!(resumed.run(&mut Quiet, u64::MAX).unwrap(), straight);
    assert!(matches!(straight, Step::Ended(End::Finished(_))));
}

fn start() -> Start {
    Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Default::default(),
    }
}

/// FIG-5887 §9: a parked graph state resumes only on the exact release
/// manifest it pins, on a worker holding every native the manifest records,
/// and with each wait's admission. Each refusal is the kernel's typed one.
#[test]
fn a_resume_refuses_a_wrong_release_a_missing_native_or_a_missing_admission() {
    let embedding = embedding(&WorkerTuning::standard()).unwrap();
    let document = document(
        &embedding,
        "main {\n  let n = num.add(1, 2)\n  let r = perform echo(n) as Any\n  return r\n}\n",
    );
    let mut machine =
        KernelMachine::start(program(&embedding, &document), BOUNDS, start()).unwrap();
    let requests = park(&mut machine);
    let state = export(&embedding, &mut machine, &document);
    let releases = embedding.release_manifests().unwrap();
    let registry = Arc::clone(program(&embedding, &document).library.registry());
    let resume = |admissions: Vec<lash_vm_protocol::EncodedPayload>,
                  releases: &ReleaseGraph,
                  registry: &FunctionRegistry| {
        import_parked(
            program(&embedding, &document),
            BOUNDS,
            &ParkedGraph {
                state: state.clone(),
                admissions,
            },
            releases,
            registry,
            limits(),
        )
        .map(|_| ())
    };
    assert_eq!(resume(admitted(&requests), releases, &registry), Ok(()));

    // Another build's releases: the pinned manifest is not held, and no
    // release of the same name stands in for it.
    let pinned = releases.manifests.current(DIALECT).unwrap();
    let foreign = ReleaseGraph {
        manifests: HeldManifests::default(),
        nodes: releases.nodes.clone(),
    };
    assert_eq!(
        resume(admitted(&requests), &foreign, &registry),
        Err(GraphStateFailure::Restore(
            KernelRestoreRefusal::ReleaseMismatch {
                pinned: NodeId(pinned.0).to_hex(),
            }
        ))
    );

    // A worker holding every definition and no native implementation.
    let mut declarations = FunctionRegistry::declarations();
    let natives: BTreeSet<_> = registry
        .iter()
        .filter(|(_, registered)| registered.native().is_some())
        .map(|(id, _)| *id)
        .collect();
    // Each function after those its body calls: register in passes until
    // a pass registers nothing more.
    let mut pending: Vec<_> = registry.iter().collect();
    loop {
        let before = pending.len();
        pending.retain(|(_, registered)| {
            declarations
                .register(registered.definition().as_ref().clone(), None)
                .is_err()
        });
        if pending.is_empty() || pending.len() == before {
            break;
        }
    }
    let Err(GraphStateFailure::Restore(KernelRestoreRefusal::MissingNative { function })) =
        resume(admitted(&requests), releases, &declarations)
    else {
        panic!("a missing native refuses resume");
    };
    assert!(natives.iter().any(|native| native.to_string() == function));

    assert!(matches!(
        resume(Vec::new(), releases, &registry),
        Err(GraphStateFailure::Restore(
            KernelRestoreRefusal::AdmissionUnresolved { .. }
        ))
    ));
    let mut twice = admitted(&requests);
    twice.extend(admitted(&requests));
    assert!(matches!(
        resume(twice, releases, &registry),
        Err(GraphStateFailure::Restore(
            KernelRestoreRefusal::Malformed { .. }
        ))
    ));
}
