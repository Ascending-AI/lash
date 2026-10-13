use lash_core::tool_run::{MaterialOwner, MaterialRole};
use lash_core::{
    EngineAction, EngineEvent, EngineState, Material, SettledOutput, StepName, StepRequest,
};
use lash_kernel_doc::{DocumentId, EffectIdentity, Site, TaskIdentity, Unit};
use lash_vm_client::VmOwner;
use lash_vm_client::wire::OutcomeWire;

use super::advance::{advance, decode, state_format};
use super::state::{
    Admission, Issued, KERNEL_RUN_STEP, KernelRunInput, KernelRunOutput, Phase, SealedRun, refused,
};
use lash_kernel_doc::KernelVersion;
use lash_kernel_state::{AdmittedRequest, AdmittedWait, ParkedRun, Run};
use lash_vm_broker::kernel::state::{OpenedRun, capture_whole};
use std::collections::{BTreeMap, BTreeSet};

fn process() -> lash_core::ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(1)
}

fn completed(text: String) -> SettledOutput {
    SettledOutput::Completed(Material::journal_local(
        MaterialOwner::Process {
            process_id: process(),
        },
        MaterialRole::AttemptOutput,
        text,
    ))
}

/// A parked run of this build's kernel version as sealed graph state: what
/// a worker exports.
fn sealed() -> SealedRun {
    sealed_stating(crate::LASH_KERNEL_VERSION)
}

/// A parked run whose whole states kernel version `stated`, sealed under
/// this build's.
fn sealed_stating(stated: u32) -> SealedRun {
    let document = DocumentId::from_bytes([7; 32]);
    let run = ParkedRun {
        run: Run {
            kernel: stated,
            document,
            functions: BTreeSet::new(),
            charged: 0,
            objects_allocated: 0,
            waits_issued: 0,
            ready: lash_kernel_state::ReadyQueue::default(),
            withdrawn: BTreeSet::new(),
            unreported: Vec::new(),
            reads: BTreeSet::new(),
        },
        session: BTreeMap::new(),
        tasks: Vec::new(),
        objects: BTreeMap::new(),
    };
    let whole = capture_whole(
        &OpenedRun {
            run,
            release: lash_vm_broker::kernel::state::release_manifests()
                .expect("the build's releases")
                .manifests
                .current(lash_vm_releases::TYPESCRIPT)
                .expect("the build holds a TypeScript release"),
            binding: lash_core_store::checkpoint_graph::bodies::OwnerBinding::Process {
                process: "law".to_owned(),
            },
        },
        u64::MAX,
    )
    .expect("a parked run stages");
    let whole = lash_vm_broker::kernel::state::WholeState {
        kernel: crate::LASH_KERNEL_VERSION,
        ..whole
    };
    SealedRun(whole.sealed(VmOwner::new("process:law"), &document.to_string()))
}

/// KMIGRATE's stale-seal check (FIG-5793) where a process opens its parked
/// run: a run whose whole states another kernel version than the one it is
/// sealed under is refused, and one that states its seal's opens.
#[test]
fn a_parked_run_whose_whole_states_another_kernel_version_than_its_seal_is_refused_at_open() {
    let base = lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    let open = |run: &SealedRun| {
        let whole = super::migrate::unsealed(run).expect("the sealed run is whole");
        super::migrate::open_run(
            &whole,
            &[],
            &base,
            lash_core_store::checkpoint_graph::GraphReadPolicy::standard().execution(),
        )
        .map(drop)
    };
    open(&sealed()).expect("a run that states its seal opens");
    let refused = open(&sealed_stating(crate::LASH_KERNEL_VERSION + 1))
        .expect_err("a run that states another kernel version is refused");
    assert!(
        refused.contains(&format!(
            "sealed under kernel version {}, and its whole states {}",
            crate::LASH_KERNEL_VERSION,
            crate::LASH_KERNEL_VERSION + 1
        )),
        "{refused}"
    );
}

/// FIG-5793's saved-process coverage, with the graph decoder's open boundary:
/// every process phase that retains a run can reopen its sealed graph, and
/// refuses one whose whole states a different kernel version than its seal.
#[test]
fn every_engine_state_holding_a_run_is_read_against_its_seal() {
    let mut written = Vec::new();
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(
            vec![
                effect(1, element(0)),
                Issued::Sleep {
                    wait: 2,
                    admission: admitted(element(1)),
                    until_ms: 5_000,
                },
            ],
            vec![],
        ),
    );
    written.push(state.clone());
    let EngineAction::Steps { steps, .. } = &action else {
        panic!("a park on an effect asks for its step, found {action:?}");
    };
    let effect_step = steps[0].step().clone();
    let (state, action) = settle(state, &effect_step, completed("\"a\"".to_owned()));
    let (run1, _, _) = run_of(&action);
    written.push(state.clone());
    let (state, action) = settle(state, &run1, parked(vec![], vec![]));
    assert!(matches!(action, EngineAction::Sleep { .. }));
    written.push(state.clone());
    let (state, action) =
        advance(KernelVersion::NEWEST, state, EngineEvent::Woke).expect("a wake advances");
    let (run2, _, _) = run_of(&action);
    written.push(state.clone());
    let outcome = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!("done")),
    );
    let (state, _) = settle(
        state,
        &run2,
        completed(
            serde_json::to_string(&KernelRunOutput::Ended {
                outcome: Box::new(outcome),
            })
            .expect("a run output serializes"),
        ),
    );
    written.push(state);

    let phases = |phase: &Phase| match phase {
        Phase::Running { .. } => "running",
        Phase::Parked => "parked",
        Phase::Ended => "ended",
    };
    let base = lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    let mut seen = Vec::new();
    for state in written {
        let (mut decoded, _) = decode(KernelVersion::NEWEST, &state)
            .unwrap_or_else(|error| panic!("a written state decodes: {error:?}"));
        assert!(decoded.parked.is_some(), "the state holds its run");
        seen.push(phases(&decoded.phase));

        decoded.parked = Some(sealed_stating(crate::LASH_KERNEL_VERSION + 1));
        let stale =
            super::advance::encode(&decoded, KernelVersion::NEWEST).expect("a state encodes");
        let (decoded, _) = decode(KernelVersion::NEWEST, &stale)
            .unwrap_or_else(|error| panic!("the saved process state decodes: {error:?}"));
        let whole =
            super::migrate::unsealed(decoded.parked.as_ref().expect("the state holds its run"))
                .expect("the sealed run is a whole state");
        let refused = super::migrate::open_run(
            &whole,
            &[],
            &base,
            lash_core_store::checkpoint_graph::GraphReadPolicy::standard().execution(),
        )
        .expect_err("a run whose whole states another version is refused");
        assert!(
            refused.contains(&format!(
                "sealed under kernel version {}, and its whole states {}",
                crate::LASH_KERNEL_VERSION,
                crate::LASH_KERNEL_VERSION + 1
            )),
            "{refused}"
        );
    }
    assert_eq!(
        seen,
        vec!["parked", "running", "parked", "running", "ended"],
        "every process phase that holds a run is read"
    );
}

/// The admission of a `perform` of `fetch` at `identity`.
fn admitted(identity: EffectIdentity) -> Admission {
    Admission(AdmittedWait {
        identity,
        request: AdmittedRequest::Effect {
            effect: lash_kernel_doc::EffectName::new("fetch").expect("a name"),
            args: Vec::new(),
        },
    })
}

fn parked(issued: Vec<Issued>, withdrawn: Vec<u64>) -> SettledOutput {
    completed(
        serde_json::to_string(&KernelRunOutput::Parked {
            state: sealed(),
            issued,
            withdrawn,
        })
        .expect("a run output serializes"),
    )
}

/// The effect a fan-out element performs: the one site, run by the task
/// the `element`-th execution of the spawn started.
fn element(element: u64) -> EffectIdentity {
    EffectIdentity {
        task: TaskIdentity::Spawned(lash_kernel_doc::SpawnIdentity {
            parent: std::sync::Arc::new(TaskIdentity::Main),
            site: Site::new(Unit::Main, vec![0]),
            occurrence: element,
        }),
        site: Site::new(
            Unit::Function(lash_kernel_doc::Name::new("worker")),
            vec![0],
        ),
        occurrence: 0,
        loops: Vec::new(),
    }
}

fn effect(wait: u64, identity: EffectIdentity) -> Issued {
    Issued::Effect {
        wait,
        admission: admitted(identity),
        effect: lash_kernel_doc::EffectName::new("tools.fetch").expect("an effect name"),
        input: serde_json::json!({ "wait": wait }),
    }
}

fn settle(
    state: EngineState,
    step: &StepName,
    outcome: SettledOutput,
) -> (EngineState, EngineAction) {
    advance(
        KernelVersion::NEWEST,
        state,
        EngineEvent::StepSettled {
            call_id: None,
            step: step.clone(),
            outcome,
        },
    )
    .expect("a settled step advances")
}

/// The one `kernel_run` an action asks for, with what it delivers, and the
/// effect steps beside it.
fn run_of(action: &EngineAction) -> (StepName, KernelRunInput, Vec<StepRequest>) {
    let EngineAction::Steps { steps, .. } = action else {
        panic!("expected steps, found {action:?}");
    };
    let mut run = None;
    let mut effects = Vec::new();
    for step in steps {
        match step {
            StepRequest::Engine { step, kind, input } => {
                assert_eq!(kind.0, KERNEL_RUN_STEP);
                run = Some((
                    step.clone(),
                    serde_json::from_value(input.clone()).expect("a run input decodes"),
                ));
            }
            tool @ StepRequest::Tool { .. } => effects.push(tool.clone()),
        }
    }
    let (step, input) = run.expect("the action runs the machine");
    (step, input, effects)
}

fn started() -> (EngineState, StepName) {
    let (state, action) = advance(
        KernelVersion::NEWEST,
        EngineState::empty(state_format(KernelVersion::NEWEST)),
        EngineEvent::Started {
            payload: serde_json::json!({
                "document": DocumentId::from_bytes([7; 32]),
                "entry": "main_entry",
            }),
        },
    )
    .expect("a start advances");
    let (run, input, effects) = run_of(&action);
    assert!(input.parked.is_none() && input.deliver.is_empty() && effects.is_empty());
    (state, run)
}

/// Target 3 and `K-TASK-012`: a body that fans out parks on every element's
/// effect at once, each its own step at the identity the machine computed
/// (the task component tells the elements apart), and each outcome is
/// handed to the machine as it settles, in whatever order: one that settles
/// while a run is in flight is handed to the next run, and none is handed
/// twice. Each run resumes with the admission of every wait the parked run
/// still holds, and no other: the state holds a wait as its identity alone.
#[test]
fn a_fan_out_waits_on_every_element_and_delivers_each_outcome_once_in_any_order() {
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(vec![effect(10, element(0)), effect(11, element(1))], vec![]),
    );
    let EngineAction::Steps { steps, wake: None } = &action else {
        panic!("a park on two effects asks for two steps, found {action:?}");
    };
    let sites: Vec<_> = steps
        .iter()
        .map(|step| {
            step.site()
                .expect("an effect step names its identity")
                .clone()
        })
        .collect();
    assert_eq!(sites, vec![element(0), element(1)]);
    assert_ne!(sites[0].task, sites[1].task);
    let (first, second) = (steps[0].step().clone(), steps[1].step().clone());

    // The second element settles first: the machine runs with it alone.
    let (state, action) = settle(state, &second, completed("\"b\"".to_owned()));
    let (run1, input, effects) = run_of(&action);
    assert!(effects.is_empty());
    assert_eq!(
        input.deliver.iter().map(|d| d.wait).collect::<Vec<_>>(),
        vec![11]
    );
    assert!(input.parked.is_some());
    let held = |input: &KernelRunInput| {
        input
            .admissions
            .iter()
            .map(|admission| admission.identity().clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(held(&input), vec![element(0), element(1)]);

    // The first settles while that run is in flight: nothing new is asked.
    let (state, action) = settle(state, &first, completed("\"a\"".to_owned()));
    assert_eq!(action, EngineAction::Idle);

    // The run parks having asked for nothing: the queued outcome runs it
    // again, and it is the only one delivered.
    let (state, action) = settle(state, &run1, parked(vec![], vec![]));
    let (run2, input, _) = run_of(&action);
    assert_eq!(
        input.deliver.iter().map(|d| d.wait).collect::<Vec<_>>(),
        vec![10]
    );
    assert_eq!(
        held(&input),
        vec![element(0)],
        "a consumed wait's admission goes"
    );
    assert_eq!(
        input.deliver[0].outcome,
        OutcomeWire::Completed(lash_kernel_doc::Datum::Text("a".to_owned()))
    );

    let outcome = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!(["a", "b"])),
    );
    let (state, action) = settle(
        state,
        &run2,
        completed(
            serde_json::to_string(&KernelRunOutput::Ended {
                outcome: Box::new(outcome.clone()),
            })
            .expect("a run output serializes"),
        ),
    );
    assert_eq!(action, EngineAction::Terminal(outcome));
    assert_eq!(
        decode(KernelVersion::NEWEST, &state)
            .expect("the state decodes")
            .0
            .phase,
        Phase::Ended
    );
}

/// An effect the boundary refuses and a withdrawn wait dispatch nothing:
/// the refusal is raised at its `perform` by the next run, a `sleep` is a
/// timer the process wakes for, and the step of a wait the machine
/// withdrew settles into nothing.
#[test]
fn a_refused_effect_is_raised_without_a_step_and_a_withdrawn_wait_is_never_delivered() {
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(
            vec![
                effect(1, element(0)),
                Issued::Refused {
                    wait: 2,
                    admission: admitted(element(1)),
                    kind: super::EFFECT_UNKNOWN.to_owned(),
                    message: "no such effect".to_owned(),
                },
                Issued::Sleep {
                    wait: 3,
                    admission: admitted(element(2)),
                    until_ms: 5_000,
                },
            ],
            vec![],
        ),
    );
    let (run1, input, effects) = run_of(&action);
    assert_eq!(effects.len(), 1);
    let withdrawn_step = effects[0].step().clone();
    assert_eq!(
        input
            .deliver
            .iter()
            .map(|d| (d.wait, d.outcome.clone()))
            .collect::<Vec<_>>(),
        vec![(
            2,
            refused(super::EFFECT_UNKNOWN, "no such effect".to_owned())
        )]
    );
    let EngineAction::Steps { wake, .. } = &action else {
        unreachable!()
    };
    assert_eq!(*wake, Some(lash_core::durable_port::DurableInstant(5_000)));

    // The machine caught the refusal and withdrew its wait on the effect.
    let (state, action) = settle(state, &run1, parked(vec![], vec![1]));
    assert_eq!(
        action,
        EngineAction::Sleep {
            until: lash_core::durable_port::DurableInstant(5_000),
            site: None,
        }
    );
    let (state, action) = settle(state, &withdrawn_step, completed("null".to_owned()));
    assert!(matches!(action, EngineAction::Sleep { .. }));

    let (_, action) =
        advance(KernelVersion::NEWEST, state, EngineEvent::Woke).expect("a wake advances");
    let (_, input, _) = run_of(&action);
    assert_eq!(
        input
            .deliver
            .iter()
            .map(|d| (d.wait, d.outcome.clone()))
            .collect::<Vec<_>>(),
        vec![(3, OutcomeWire::Elapsed)]
    );
}

/// Workflow admission's local names preserve the wrapper and every checker
/// error; hosts never need to parse a payload or display message for the code.
#[test]
fn workflow_admission_refusals_have_stable_local_codes() {
    use super::workflow::WorkflowAdmissionRefusal;
    let document = lash_kernel_doc::parse_document(
        "kernel 1\nnumbers float\neffect fetch(url: Text) -> Any\neffect store(key: Text) -> Any\n\nmain {\n  let a = perform fetch(\"u\") as Any\n  let b = perform store(\"k\") as Any\n}\n",
    ).expect("document");
    let functions = BTreeMap::new();
    let refusal =
        lash_kernel_check::admit(&document, &lash_kernel_check::Environment::new(&functions))
            .expect_err("both effects are missing");
    let document_refusal = WorkflowAdmissionRefusal::Document(refusal);
    assert_eq!(document_refusal.code(), "document");
    let WorkflowAdmissionRefusal::Document(refusal) = document_refusal else {
        panic!("the wrapper retains the checker refusal");
    };
    assert_eq!(refusal.errors.len(), 2);
    assert_eq!(
        refusal
            .errors
            .iter()
            .map(|error| error.reason.code())
            .collect::<Vec<_>>(),
        ["missing_effect", "missing_effect"]
    );
    assert_eq!(
        WorkflowAdmissionRefusal::NoSuchEntry {
            entry: lash_kernel_doc::Name::new("missing"),
        }
        .code(),
        "no_such_entry"
    );
    assert_eq!(
        WorkflowAdmissionRefusal::Boundary {
            message: "catalog cannot be stated as effects".into()
        }
        .code(),
        "boundary"
    );
}

#[cfg(feature = "synthetic-next")]
mod format_carry {
    use std::sync::Arc;

    use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphReadPolicy, GraphStaging, NodeKind, TypedBody,
    };
    use lash_kernel_doc::graph_tokens::ChildRef;
    use lash_kernel_doc::{Datum, Document, FunctionId, FunctionRegistry, Integer, Timestamp};
    use lash_kernel_state::{AdmittedRequest, AdmittedWait};
    use lash_kernel_vm::{
        Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
        Request, Start, Step, Target, WaitId,
    };
    use lash_vm_broker::kernel::state::{
        OpenedRun, ReleaseGraph, WholeState, capture_whole, import_whole_state, open_whole,
    };
    use lash_vm_releases::{HeldManifests, HeldReleases, HelperRelease, ReleasedFunction};

    use super::super::migrate::{
        KernelMigrationRefusal, PlannedMigration, migrate_run_with_releases, plan_migration,
    };

    struct NoHost;

    impl Host for NoHost {
        fn clock(&mut self) -> Timestamp {
            Timestamp {
                nanoseconds: Integer::from(0),
            }
        }
        fn random(&mut self) -> u64 {
            0
        }
        fn read(
            &mut self,
            _handle: &lash_kernel_doc::Handle,
            _request: &Datum,
        ) -> Result<Datum, lash_kernel_doc::ErrorDatum> {
            Ok(Datum::Null)
        }
        fn print(&mut self, _value: &Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }

    struct Parked {
        base: Document,
        plan: PlannedMigration,
        whole: WholeState,
        admitted: Vec<AdmittedWait>,
        wait: WaitId,
        bounds: Bounds,
        library: Arc<FunctionRegistry>,
        releases: ReleaseGraph,
        r1: ChildRef,
        r2: ChildRef,
        helper: FunctionId,
    }

    /// The build retains R1's exact body and writes R2's changed body. The
    /// compatible R1 holds its body in both kernel encodings. An incompatible
    /// R1 holds only the kernel-1 body, even though the registry can convert it.
    fn parked(compatible: bool) -> Parked {
        let mut library = FunctionRegistry::new();
        let mut released = Vec::new();
        for value in ["R1", "R2"] {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function helper() -> Text\nkernel 1\ncharge 1\nbody {{ return \"{value}\" }}\n"
            ))
            .expect("a released helper definition");
            let function = library
                .register(definition.clone(), None)
                .expect("the helper registers");
            released.push(ReleasedFunction {
                function,
                definition,
                fingerprint: None,
            });
        }
        let helper = released[0].function;
        let mut retained = vec![released[0].clone()];
        if compatible {
            let definition = lash_kernel_doc::FunctionDefinition {
                kernel: lash_kernel_doc::KernelVersion::SyntheticNext.number(),
                ..released[0].definition.clone()
            };
            retained.push(ReleasedFunction {
                function: definition
                    .identity()
                    .expect("the converted body has an identity"),
                definition,
                fingerprint: None,
            });
        }
        let r1 = HelperRelease {
            release: "R1".to_owned(),
            ordinal: 1,
            sealed: true,
            writes: [helper].into(),
            functions: retained.clone(),
        };
        let current = released[1].function;
        retained.push(released[1].clone());
        let current_encoding = lash_kernel_doc::FunctionDefinition {
            kernel: lash_kernel_doc::KernelVersion::SyntheticNext.number(),
            ..released[1].definition.clone()
        };
        retained.push(ReleasedFunction {
            function: current_encoding
                .identity()
                .expect("R2's next-kernel identity"),
            definition: current_encoding,
            fingerprint: None,
        });
        let r2 = HelperRelease {
            release: "R2".to_owned(),
            ordinal: 2,
            sealed: false,
            writes: [current].into(),
            functions: retained,
        };
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(1024 * 1024)));
        let mut manifests = HeldManifests::default();
        manifests
            .stage(
                "typescript",
                &HeldReleases {
                    retained: vec![r1, r2],
                    retiring: Vec::new(),
                },
                |kind, logical| {
                    staging
                        .encode(NodeKind(kind), TypedBody::logical(logical))
                        .map(|id| id.child_ref())
                },
            )
            .expect("both releases stage");
        let r1 = manifests
            .of_dialect("typescript")
            .find(|(_, manifest)| manifest.release == "R1")
            .expect("R1 is retained")
            .0;
        let r2 = manifests.current("typescript").expect("R2 is current");
        let releases = ReleaseGraph {
            manifests,
            nodes: staging.into_nodes(),
        };
        let body = format!(
            "use helper = @{helper}\nmain {{ do sleep 1 let value = invoke helper() finish value }}"
        );
        let base = lash_kernel_doc::parse_document(&format!("kernel 1\nnumbers float\n{body}\n"))
            .expect("the process document parses");
        lash_kernel_migrate::migrate_registry(
            &mut library,
            lash_kernel_migrate::migration_from(lash_kernel_doc::KernelVersion::One)
                .expect("the synthetic format migration"),
        )
        .expect("the library holds both kernel versions");
        let library = Arc::new(library);
        let plan = plan_migration(&base, &library)
            .expect("the document carries")
            .expect("v1 carries");
        let bounds = Bounds {
            charge: 10_000,
            memory: 1024 * 1024,
            call_depth: 32,
            live_tasks: 16,
            requests_per_park: 16,
            join_members: 16,
        };
        let mut machine = KernelMachine::start(
            Program {
                document: Arc::new(base.clone()),
                library: PreparedLibrary::new(Arc::clone(&library)),
            },
            bounds,
            Start {
                target: Target::Main,
                args: Vec::new(),
                bindings: Bindings::default(),
            },
        )
        .expect("the v1 run starts");
        let Step::Parked(park) = machine.run(&mut NoHost, u64::MAX).expect("the run parks") else {
            panic!("the run must wait");
        };
        let [Request::Sleep(sleep)] = park.requests.as_slice() else {
            panic!("one admitted sleep");
        };
        let admitted = vec![AdmittedWait {
            identity: sleep.identity.clone(),
            request: AdmittedRequest::Sleep {
                duration: sleep.duration.into(),
            },
        }];
        let whole = capture_whole(
            &OpenedRun {
                run: machine.export().expect("the run exports"),
                release: r1,
                binding: OwnerBinding::Process {
                    process: "format-carry".to_owned(),
                },
            },
            1024 * 1024,
        )
        .expect("the park captures R1");
        Parked {
            base,
            plan,
            whole,
            admitted,
            wait: sleep.wait,
            bounds,
            library,
            releases,
            r1,
            r2,
            helper,
        }
    }

    /// FIG-6127 / R-12 / D2: converting the kernel format preserves R1's
    /// manifest even when the carrying build writes R2 and retains R1.
    #[test]
    fn a_kernel_format_carry_keeps_the_retained_release_and_resumes_on_it() {
        let parked = parked(true);
        assert_ne!(parked.r1, parked.r2);
        let limits = GraphReadPolicy::standard().execution();
        let carried = migrate_run_with_releases(
            &parked.whole,
            &parked.admitted,
            &parked.base,
            &parked.plan,
            limits,
            &parked.releases,
        )
        .expect("the format carries");
        assert_eq!(
            carried.kernel,
            lash_kernel_doc::KernelVersion::SyntheticNext.number()
        );
        let admitted: Vec<_> = parked
            .admitted
            .into_iter()
            .map(|mut wait| {
                wait.identity = parked
                    .plan
                    .rewritten
                    .effect_identity(&parked.base, &wait.identity)
                    .expect("the admitted wait carries");
                wait
            })
            .collect();
        let opened = open_whole(
            &carried,
            &parked.releases,
            &admitted,
            parked
                .plan
                .rewritten
                .document
                .manifest
                .functions
                .keys()
                .copied()
                .collect(),
            limits,
        )
        .expect("the carried run opens");
        assert_eq!(
            opened.release, parked.r1,
            "a format carry must not select R2"
        );
        let (mut machine, release) = import_whole_state(
            Program {
                document: Arc::new(parked.plan.rewritten.document),
                library: PreparedLibrary::new(parked.library.clone()),
            },
            parked.bounds,
            carried.root,
            &carried.nodes,
            admitted,
            &parked.releases,
            &parked.library,
            limits,
        )
        .expect("the retained release resumes the carried state");
        assert_eq!(release, parked.r1);
        machine
            .deliver(parked.wait, Outcome::Elapsed)
            .expect("the same wait settles");
        let Step::Ended(End::Finished(answer)) =
            machine.run(&mut NoHost, u64::MAX).expect("resumed")
        else {
            panic!("the carried process must finish");
        };
        assert_eq!(answer.result, Datum::Text("R1".to_owned()));
    }

    /// FIG-6127: a pinned helper definition that cannot serve the new
    /// kernel format refuses the carry with a typed reason naming R1.
    #[test]
    fn a_kernel_format_carry_refuses_helpers_the_pinned_release_cannot_supply() {
        let parked = parked(false);
        let refusal = migrate_run_with_releases(
            &parked.whole,
            &parked.admitted,
            &parked.base,
            &parked.plan,
            GraphReadPolicy::standard().execution(),
            &parked.releases,
        )
        .expect_err("the format must not substitute helpers or escape the pinned manifest");
        let KernelMigrationRefusal::Parked {
            refusal: lash_kernel_migrate::ParkedRefusal::StateNotCarried { detail },
            ..
        } = refusal
        else {
            panic!("a typed incompatible-release refusal: {refusal:?}");
        };
        assert!(detail.contains("R1"), "{detail}");
        assert!(detail.contains(&parked.helper.to_string()), "{detail}");

        // The same carry must refuse an unavailable manifest, even when the
        // library still holds its definitions, rather than selecting R2.
        let mut missing = parked.releases;
        missing.manifests = HeldManifests::default();
        let refusal = migrate_run_with_releases(
            &parked.whole,
            &parked.admitted,
            &parked.base,
            &parked.plan,
            GraphReadPolicy::standard().execution(),
            &missing,
        )
        .expect_err("an unavailable pinned release refuses the format carry");
        let KernelMigrationRefusal::Parked {
            refusal: lash_kernel_migrate::ParkedRefusal::StateNotCarried { detail },
            ..
        } = refusal
        else {
            panic!("a typed missing-release refusal: {refusal:?}");
        };
        assert!(
            detail.contains(&lash_core_store::checkpoint_graph::NodeId(parked.r1.0).to_hex()),
            "{detail}"
        );
    }
}

/// P2: admitting an entry does not require the effects of main or siblings.
#[test]
fn workflow_admission_checks_only_the_selected_entry() {
    let document = lash_kernel_doc::parse_document(
        r#"kernel 1
numbers float
effect a.run(input: Any) -> Any
effect b.run(input: Any) -> Any
entry worker() -> Any
fn worker() { let got = perform b.run(null) as Any return got }
main { let got = perform a.run(null) as Any finish got }
"#,
    )
    .unwrap();
    let functions = BTreeMap::new();
    let mut environment = lash_kernel_check::Environment::new(&functions);
    environment.effects.insert(
        lash_kernel_doc::EffectName::new("b.run").unwrap(),
        document.manifest.effects[&lash_kernel_doc::EffectName::new("b.run").unwrap()].clone(),
    );
    super::WorkflowDocument::admit(
        document,
        lash_trace::WorkflowDocumentEntry::Entry {
            function: lash_kernel_doc::Name::new("worker"),
        },
        &environment,
    )
    .expect("only worker's performed effect must be offered");
}
