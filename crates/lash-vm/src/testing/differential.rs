//! Differential execution: one program admitted two ways must look the same
//! to a host.
//!
//! [`run_artifact`] runs every entry of an admitted module (its `main` and
//! each exported process) against a [`DeterministicHost`] and keeps what the
//! host saw: each entry's outcome or runtime failure, every ability the VM
//! asked for in order, every execution observation it emitted, and the state
//! the run left. [`ArtifactRun::differences`] compares two such runs.
//!
//! [`document_runs_like_its_source`] is the law built on it: it admits one
//! program through its source and through its workflow document and requires
//! the runs to agree, so a document that reconstructs something its source
//! did not say is caught by what it does and not only by how it is spelled.
//! The generated-program laws and the dialect's corpus laws both call it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_sansio::sync::MutexExt;

use crate::{
    AbilityOp, AbilityOutcome, Declaration, Entry, ExecutionBound, ExecutionBounds,
    ExecutionEnvironment, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    LashVmExecutionCallSite, LashVmExecutionObservation, LashVmHostEnvironment, LinkedModule,
    ModuleArtifact, Program, Record, ResourceOperation, ResourceOperationBatchLeaf,
    ResourceOperationOutcome, RuntimeError, State, TypeExpr, Value, WorkflowAdmission,
    WorkflowDeclaration, WorkflowDraft, WorkflowGraph, admit_workflow_graph,
    workflow_graph_from_artifact, workflow_graph_from_program,
};

/// What one run may spend. A program that never ends (an edit can make one
/// of a loop) stops on the instruction budget, the same way on every run.
const RUN_BOUNDS: ExecutionBounds = ExecutionBounds::new(
    ExecutionBound::instructions(200_000),
    ExecutionBound::logical_bytes(64 * 1024 * 1024),
);

/// A host whose every answer is a function of the request and of how many
/// processes the run started before it, and which keeps what it was asked.
///
/// - `echo` answers the `value` field of its argument, `err` and `fail` fail,
///   `start` answers a process handle carrying the `value` of its `args`,
///   `cancel` answers null, and every other operation answers its first
///   argument.
/// - Awaiting a handle answers the value the handle carries.
/// - `print`, `sleep`, `finish` and `fail` succeed.
/// - A run is bounded: see [`RUN_BOUNDS`].
#[derive(Default)]
pub struct DeterministicHost {
    effects: Mutex<Vec<serde_json::Value>>,
    calls: Mutex<Vec<ToolCall>>,
    observations: Mutex<Vec<LashVmExecutionObservation>>,
    started: AtomicU64,
}

impl DeterministicHost {
    fn resource_operation(
        &self,
        operation: &ResourceOperation,
    ) -> Result<Value, ExecutionHostError> {
        let argument = operation.args.first();
        self.calls.lock_recover().push(ToolCall {
            operation: operation.operation.clone(),
            argument: argument.cloned(),
            site: operation.call_site.as_deref().cloned(),
        });
        let field = |name: &str| {
            argument
                .and_then(Value::as_record)
                .and_then(|record| record.get(name))
                .cloned()
        };
        match operation.operation.as_str() {
            "echo" => Ok(field("value").unwrap_or(Value::Null)),
            "err" | "fail" => Err(ExecutionHostError::new("boom")),
            "start" => {
                let ordinal = self.started.fetch_add(1, Ordering::Relaxed) + 1;
                let process = lash_sansio::ProcessId::fixture(&format!("proc-{ordinal}"));
                let carried = field("args")
                    .as_ref()
                    .and_then(Value::as_record)
                    .and_then(|args| args.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut handle = Record::default();
                handle.insert(
                    lash_sansio::handle::HANDLE_FIELD.to_string(),
                    Value::String(lash_sansio::handle::HANDLE_KIND.into()),
                );
                handle.insert(
                    "id".to_string(),
                    Value::String(
                        lash_sansio::handle::HandleId::process(&process)
                            .as_str()
                            .into(),
                    ),
                );
                handle.insert("value".to_string(), carried);
                Ok(Value::Record(handle.into()))
            }
            "cancel" => Ok(Value::Null),
            _ => Ok(argument.cloned().unwrap_or(Value::Null)),
        }
    }
}

impl ExecutionHost for DeterministicHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        self.effects.lock_recover().push(
            serde_json::to_value(&op)
                .unwrap_or_else(|error| serde_json::Value::String(error.to_string())),
        );
        match op {
            AbilityOp::ResourceOperation(operation) => self
                .resource_operation(&operation)
                .map(AbilityOutcome::Value),
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = batch
                    .leaves
                    .iter()
                    .map(|leaf| match leaf {
                        ResourceOperationBatchLeaf::Operation(operation) => {
                            ResourceOperationOutcome::from_result(
                                self.resource_operation(operation),
                            )
                        }
                        ResourceOperationBatchLeaf::Timer(_) => {
                            ResourceOperationOutcome::Value(Value::Undefined)
                        }
                    })
                    .collect();
                Ok(AbilityOutcome::ResourceOperationBatch(
                    batch.answer_in_leaf_order(results),
                ))
            }
            AbilityOp::Await(awaited) => awaited
                .handle
                .as_record()
                .map(|record| {
                    AbilityOutcome::Value(record.get("value").cloned().unwrap_or(Value::Null))
                })
                .ok_or_else(|| ExecutionHostError::new("expected handle record")),
            AbilityOp::Print(_) => Ok(AbilityOutcome::Unit),
            AbilityOp::Sleep(_) => Ok(AbilityOutcome::Value(Value::Null)),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityOutcome::Value(value)),
        }
    }

    fn projected_bindings(&self) -> crate::ProjectedBindings {
        crate::testing::projection::reading_test_views(crate::ProjectedBindings::new())
    }

    fn observes_lash_vm_execution(&self) -> bool {
        true
    }

    fn observe_lash_vm_execution(&self, observation: LashVmExecutionObservation) {
        self.observations.lock_recover().push(observation);
    }
}

/// One module operation a run asked the host for.
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub operation: String,
    /// The operation's first argument.
    pub argument: Option<Value>,
    /// The site and occurrence the VM attributed the call to.
    pub site: Option<LashVmExecutionCallSite>,
}

/// One entry of a module, run once.
#[derive(Clone, Debug)]
pub struct EntryRun {
    /// `main`, or the name of the exported process.
    pub entry: String,
    /// How the run ended: an outcome, or the runtime failure that stopped it.
    pub outcome: Result<ExecutionOutcome, RuntimeError>,
    /// Every ability the VM asked the host for, in order.
    pub effects: Vec<serde_json::Value>,
    /// Every module operation among [`Self::effects`], in order, as typed
    /// values.
    pub calls: Vec<ToolCall>,
    /// Every execution observation the VM emitted, in order.
    pub observations: Vec<LashVmExecutionObservation>,
    /// The canonical snapshot of the state the run left, or why the state
    /// has none.
    pub state: Result<Vec<u8>, String>,
}

/// Every entry of one admitted module, run once each.
#[derive(Clone, Debug)]
pub struct ArtifactRun {
    pub entries: Vec<EntryRun>,
}

impl ArtifactRun {
    /// Where this run and `other` differ, as one line per difference; empty
    /// when a host cannot tell them apart.
    ///
    /// Values are compared as printed: a `NaN` is the same value on both
    /// sides and is never equal to itself as a number.
    pub fn differences(&self, other: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        let names = |run: &Self| {
            run.entries
                .iter()
                .map(|entry| entry.entry.clone())
                .collect::<Vec<_>>()
        };
        if names(self) != names(other) {
            differences.push(format!("entries: {:?} vs {:?}", names(self), names(other)));
            return differences;
        }
        for (left, right) in self.entries.iter().zip(&other.entries) {
            let mut differ = |what: &str, left_value: String, right_value: String| {
                if left_value != right_value {
                    differences.push(format!(
                        "{} {what}:\n  {left_value}\n  {right_value}",
                        left.entry
                    ));
                }
            };
            differ(
                "outcome",
                format!("{:?}", left.outcome),
                format!("{:?}", right.outcome),
            );
            let (ours, theirs) = first_mismatch(&left.effects, &right.effects);
            differ("effects", ours, theirs);
            let (ours, theirs) = first_mismatch(&left.observations, &right.observations);
            differ("observations", ours, theirs);
            differ(
                "state",
                format!("{:?}", left.state),
                format!("{:?}", right.state),
            );
        }
        differences
    }
}

/// The first position at which two sequences differ, as each side prints it;
/// two equal strings when they agree.
fn first_mismatch<T: std::fmt::Debug>(left: &[T], right: &[T]) -> (String, String) {
    let printed = |items: &[T], index: usize| match items.get(index) {
        Some(item) => format!("[{index}] {item:?}"),
        None => format!("[{index}] nothing ({} in all)", items.len()),
    };
    (0..left.len().max(right.len()))
        .map(|index| (printed(left, index), printed(right, index)))
        .find(|(ours, theirs)| ours != theirs)
        .unwrap_or_default()
}

/// The value a run binds a process parameter to: a function of the
/// parameter's position and declared type alone.
fn parameter_value(index: usize, ty: &TypeExpr) -> Value {
    match ty {
        TypeExpr::Int | TypeExpr::Float => Value::Number(index as f64 + 1.0),
        TypeExpr::Bool => Value::Bool(true),
        TypeExpr::Null => Value::Null,
        TypeExpr::List(_) => Value::List(Vec::new().into()),
        TypeExpr::Object(_) | TypeExpr::Dict => Value::Record(Record::default().into()),
        _ => Value::String(format!("argument-{index}").into()),
    }
}

async fn run_entry(
    artifact: &ModuleArtifact,
    name: &str,
    entry: Entry<'_>,
    parameters: &[(String, Value)],
) -> EntryRun {
    let host = DeterministicHost::default();
    let mut state = State::new();
    let process = matches!(entry, Entry::Process(_));
    let outcome = match crate::compile(artifact, entry, None) {
        Err(error) => Err(error),
        Ok(compiled) => {
            let seeded = parameters.iter().try_for_each(|(name, value)| {
                state.insert_global(name.clone(), value.clone()).map(|_| ())
            });
            match seeded {
                Err(error) => Err(error),
                Ok(()) => {
                    let environment =
                        ExecutionEnvironment::new(&host).with_execution_bounds(RUN_BOUNDS);
                    let environment = if process {
                        environment.process()
                    } else {
                        environment
                    };
                    crate::execute(&compiled, &mut state, &environment).await
                }
            }
        }
    };
    EntryRun {
        entry: name.to_string(),
        outcome,
        effects: std::mem::take(&mut *host.effects.lock_recover()),
        calls: std::mem::take(&mut *host.calls.lock_recover()),
        observations: std::mem::take(&mut *host.observations.lock_recover()),
        state: state
            .snapshot()
            .to_canonical_bytes()
            .map_err(|error| error.to_string()),
    }
}

/// Runs `main` and every exported process of `artifact`, each against a
/// fresh state and a fresh [`DeterministicHost`]. A process runs with each
/// parameter bound to a value its position and type decide.
pub async fn run_artifact(artifact: &ModuleArtifact) -> ArtifactRun {
    let mut entries = vec![run_entry(artifact, "main", Entry::Main, &[]).await];
    for declaration in &artifact.ir().declarations {
        let Declaration::Process(process) = declaration else {
            continue;
        };
        let Some(process_ref) = artifact.process_ref(&process.name) else {
            continue;
        };
        let parameters = process
            .params
            .iter()
            .enumerate()
            .map(|(index, param)| (param.name.to_string(), parameter_value(index, &param.ty)))
            .collect::<Vec<_>>();
        entries.push(
            run_entry(
                artifact,
                &process.name,
                Entry::Process(process_ref),
                &parameters,
            )
            .await,
        );
    }
    ArtifactRun { entries }
}

/// The wire encoding of a document. Two documents are the same document
/// when they encode the same: a `NaN` literal is equal to itself here, as it
/// is not under `==`.
fn encoded(graph: &WorkflowGraph) -> Result<String, String> {
    serde_json::to_string(graph).map_err(|error| error.to_string())
}

/// A document as a host holds one: through its wire encoding.
pub fn through_wire(graph: &WorkflowGraph) -> Result<WorkflowGraph, String> {
    WorkflowGraph::decode_json(&encoded(graph)?).map_err(|error| error.to_string())
}

fn admit(
    document: &WorkflowGraph,
    environment: &LashVmHostEnvironment,
) -> Result<WorkflowAdmission, String> {
    let admission = admit_workflow_graph(document, environment)
        .map_err(|refusal| format!("{refusal}: {:?}", refusal.diagnostics))?;
    // What admission says a submitted node became is a node of the document
    // it admitted.
    let admitted =
        admission
            .graph
            .nodes()
            .map(|node| &node.id)
            .chain(admission.graph.declarations.iter().filter_map(
                |declaration| match declaration {
                    WorkflowDeclaration::Process(process) => Some(&process.id),
                    WorkflowDeclaration::Function(_) => None,
                },
            ))
            .collect::<std::collections::BTreeSet<_>>();
    match admission.nodes.values().find(|id| !admitted.contains(id)) {
        Some(id) => Err(format!(
            "admission maps a node to `{id}`, which the admitted document does not hold"
        )),
        None => Ok(admission),
    }
}

/// What a host publishes from the draft document of `program`: the document
/// through its wire encoding, opened for editing, and what the draft exports
/// admitted against `environment`.
pub fn published_from_draft(
    program: &Program,
    environment: &LashVmHostEnvironment,
) -> Result<WorkflowAdmission, String> {
    let draft = WorkflowDraft::open(&through_wire(&workflow_graph_from_program(program))?)
        .map_err(|error| error.to_string())?;
    admit(draft.document(), environment)
}

/// The document of an admitted module, through its wire encoding, admitted
/// again: what a host publishes when it reads a definition and changes
/// nothing.
pub fn readmitted_from_document(
    artifact: &ModuleArtifact,
    environment: &LashVmHostEnvironment,
) -> Result<WorkflowAdmission, String> {
    admit(
        &through_wire(&workflow_graph_from_artifact(artifact))?,
        environment,
    )
}

/// One program admitted from its source and run, after its publications
/// from its document were found to run the same.
#[derive(Clone, Debug)]
pub struct SourceRun {
    pub artifact: ModuleArtifact,
    pub run: ArtifactRun,
}

/// The differential law: a program published from its workflow document
/// behaves exactly like the program admitted from its source. Every entry of
/// the module (main and each process) gives the same outcome or failure
/// value, asks the host for the same abilities in the same order, emits the
/// same execution observations and leaves the same state, and the two
/// modules have the same identity and the same admitted document.
///
/// Two publications are compared with the source: the draft document
/// ([`published_from_draft`], process literals inline) and the admitted
/// document ([`readmitted_from_document`], processes lifted).
pub async fn document_runs_like_its_source(
    program: &Program,
    environment: &LashVmHostEnvironment,
) -> Result<SourceRun, String> {
    let source =
        LinkedModule::link(program.clone(), environment).map_err(|error| error.to_string())?;
    let document = encoded(&workflow_graph_from_artifact(&source.artifact))?;
    let run = run_artifact(&source.artifact).await;
    let publications = [
        (
            "the draft document",
            published_from_draft(program, environment),
        ),
        (
            "the admitted document",
            readmitted_from_document(&source.artifact, environment),
        ),
    ];
    for (what, published) in publications {
        let published = published.map_err(|error| format!("{what} is not admitted: {error}"))?;
        let artifact = &published.linked.artifact;
        if artifact.module_ref() != source.artifact.module_ref()
            || artifact.source_identity() != source.artifact.source_identity()
        {
            return Err(format!(
                "{what} publishes a different module than its source"
            ));
        }
        if encoded(&published.graph)? != document {
            return Err(format!(
                "{what} is admitted as a different document than its source"
            ));
        }
        let differences = run.differences(&run_artifact(artifact).await);
        if !differences.is_empty() {
            return Err(format!(
                "{what} does not run like its source:\n{}",
                differences.join("\n")
            ));
        }
    }
    Ok(SourceRun {
        artifact: source.artifact,
        run,
    })
}

/// The site-attribution law: every site a run reports is a site of the
/// document of the module that ran, and no two things that ran share one.
///
/// * The node an observation or a call names is a node of that document, and
///   the document lists the site among that node's execution sites, so an
///   overlay never has to graft an observation onto the graph.
/// * The occurrences of one site count the calls the host received from it:
///   1, 2, 3 and so on, in order. Two calls that shared a site would skip a
///   number at one of them.
/// * Every loop a call says it ran inside is a site of the document too.
pub fn observed_sites_are_in_the_document(source: &SourceRun) -> Result<(), String> {
    let document = workflow_graph_from_artifact(&source.artifact);
    let sites = document
        .nodes()
        .map(|node| (node.id.clone(), &node.execution_sites))
        .collect::<BTreeMap<_, _>>();
    let listed = |entry: &str, call: &LashVmExecutionCallSite| {
        let site = &call.at.site;
        let Some(listed) = sites.get(&site.node_id) else {
            return Err(format!("{entry}: {site} names no node of the document"));
        };
        let described = lash_sansio::WorkflowSiteDescriptor {
            site_path: site.site_path.clone(),
            kind: call.kind,
            label: call.label.clone(),
        };
        if listed.contains(&described) {
            Ok(())
        } else {
            Err(format!(
                "{entry}: the document does not list {described:?} on its node; it lists {listed:?}"
            ))
        }
    };
    for entry in &source.run.entries {
        for observation in &entry.observations {
            listed(&entry.entry, &observation.call_site)?;
        }
        let mut occurrences = BTreeMap::new();
        for call in entry.calls.iter().filter_map(|call| call.site.as_ref()) {
            listed(&entry.entry, call)?;
            let count = occurrences.entry(call.at.site.clone()).or_insert(0u64);
            *count += 1;
            if call.at.occurrence.get() != *count {
                return Err(format!(
                    "{}: call {} of {} is numbered {}",
                    entry.entry, count, call.at.site, call.at.occurrence
                ));
            }
            for frame in &call.at.loops {
                let in_document = sites.get(&frame.site.node_id).is_some_and(|listed| {
                    listed
                        .iter()
                        .any(|site| site.site_path == frame.site.site_path)
                });
                if !in_document {
                    return Err(format!(
                        "{}: the loop {} around {} is no site of the document",
                        entry.entry, frame.site, call.at.site
                    ));
                }
            }
        }
    }
    Ok(())
}
