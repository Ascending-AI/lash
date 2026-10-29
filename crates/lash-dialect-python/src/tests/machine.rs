//! Runs a lowered cell on the kernel machine, with scripted tools.
//!
//! The registry is the kernel library and the dialect's helpers, nothing
//! else. A run is recorded the way `witness/record.py` records CPython: one
//! epoch per stretch between deliveries, with the outcomes delivered into
//! it, the tool calls and sleeps it requested and the lines it printed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Environment, Lowered, NamedLibrary};
use lash_kernel_doc::{Datum, ErrorDatum};

use lash_kernel_state::semantic::{self, Checkpoint};
use lash_kernel_vm::{
    Bounds, End, Host, KernelMachine, Machine, Outcome, Park, PreparedLibrary, Program, Request,
    RunError, Step, WaitId,
};
use serde::Deserialize;
use {
    lash_kernel_value::EffectName, lash_kernel_value::Name, lash_kernel_value::Param,
    lash_kernel_value::Signature, lash_kernel_value::Type,
};

use super::pages::{self, Session};

pub(super) struct Installed {
    pub(super) prepared: PreparedLibrary,
    library: NamedLibrary,
}

/// The kernel library with the dialect's helpers: the registry a machine
/// runs with and the library the front end lowers against.
pub(super) fn installed() -> &'static Installed {
    static INSTALLED: OnceLock<Installed> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let registry =
            Arc::new(lash_kernel_conformance::installed_functions().expect("installed natives"));
        let roots = lash_kernel_releases::helper_release()
            .expect("helper release")
            .resolver_roots();
        let library =
            NamedLibrary::resolving(&registry, |id| roots.contains(id)).expect("exported names");
        Installed {
            prepared: PreparedLibrary::with_helpers(
                registry,
                Arc::clone(
                    &lash_kernel_releases::helper_release()
                        .expect("build helpers")
                        .helpers,
                ),
            ),
            library,
        }
    })
}

pub(crate) fn library() -> &'static NamedLibrary {
    &installed().library
}

/// The tools a cell may call: `host.echo(x)` answers `x` and `host.boom(x)` fails
/// with an error of kind `boom` whose message is `x`.
pub(crate) fn effects() -> BTreeMap<EffectName, Signature> {
    ["host.echo", "host.boom"]
        .into_iter()
        .map(|name| {
            let signature = Signature {
                params: vec![Param {
                    name: Name::new("x"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            };
            (EffectName::new(name).expect("a tool's name"), signature)
        })
        .collect()
}

/// Lowers `source` as a cell of a session that holds `bindings` and can
/// call the tools `effects`.
fn lower_with(
    source: &str,
    effects: &BTreeMap<EffectName, Signature>,
    bindings: &BTreeSet<Name>,
) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    crate::lower(
        source,
        &Environment::new(
            library(),
            &crate::tests::linked::namespaces(effects, controls())
                .expect("the test's effects are operations"),
        ),
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment {
            bindings,
            functions: &BTreeMap::new(),
        }),
    )
}

/// The laws' turn-ending tool: `control.finish(x)` ends the turn with `x`,
/// where a law's host offers it.
pub(crate) fn controls() -> &'static std::collections::BTreeMap<
    lash_kernel_value::EffectName,
    std::collections::BTreeSet<lash_kernel_dialect::EffectControl>,
> {
    static CONTROLS: std::sync::OnceLock<
        std::collections::BTreeMap<
            lash_kernel_value::EffectName,
            std::collections::BTreeSet<lash_kernel_dialect::EffectControl>,
        >,
    > = std::sync::OnceLock::new();
    CONTROLS.get_or_init(|| {
        std::collections::BTreeMap::from([(
            lash_kernel_value::EffectName::new("control.finish").expect("a tool's name"),
            std::collections::BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
        )])
    })
}

/// Lowers `source` as a first cell whose host offers `effects`.
pub(crate) fn lower_with_effects(
    source: &str,
    effects: &BTreeMap<EffectName, Signature>,
) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    lower_with(source, effects, &BTreeSet::new())
}

/// Lowers `source` as a first cell.
pub(crate) fn lower(source: &str) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    lower_with(source, &effects(), &BTreeSet::new())
}

/// The kernel text of the document `source` lowers to.
pub(crate) fn kernel_text(source: &str) -> String {
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    let mut printed = lash_kernel_doc::print_document(&lowered.document);
    for id in lowered.document.library.iter() {
        if let Some(header) = lash_kernel_doc::FunctionCatalog::header(library(), id) {
            printed = printed.replace(&format!("@{id}"), header.name.as_str());
        }
    }
    let mut text = String::new();
    for definition in lowered.document.functions.values() {
        if lash_kernel_doc::FunctionCatalog::header(library(), &definition.identity()).is_none()
            && matches!(
                definition.implementation,
                lash_kernel_doc::Implementation::Guest {
                    metering: lash_kernel_doc::Metering::Statements,
                    ..
                }
            )
        {
            text.push_str(&lash_kernel_doc::print_definition(definition));
        }
    }
    if let Some(at) = printed.find("main {") {
        text.push_str(&printed[at..]);
    }
    text
}

/// Whether authored code invokes this exact imported helper.
pub(crate) fn calls_helper(source: &str, helper: &str) -> bool {
    use lash_kernel_dialect::Library as _;
    use lash_kernel_doc::{ActKind, Action, Callee, Expr, Node};
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    let document = &lowered.document;
    let function = library().resolve(helper).expect("known helper");
    let matches = |callee: &Callee| match callee {
        Callee::Local(id) => *id == function,
        Callee::Import(_) => false,
        _ => false,
    };
    let mut pending = document
        .units()
        .filter(|(unit, _)| match unit {
            lash_kernel_value::sites::Unit::Library(id) => {
                lash_kernel_doc::FunctionCatalog::header(library(), id).is_none()
            }
            lash_kernel_value::sites::Unit::Main => true,
            _ => false,
        })
        .map(|(_, body)| Node::Block(body))
        .collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        pending.extend(node.children());
        if match node {
            Node::Expr(Expr::Call {
                function: called, ..
            }) => *called == function,
            Node::Expr(Expr::Act(action)) => match &action.kind {
                ActKind::Call(callee) => matches(callee),
                _ => false,
            },
            Node::Action(Action::Call { callee, .. }) => matches(callee),
            _ => false,
        } {
            return true;
        }
    }
    false
}

pub(super) const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 50_000_000,
    memory: 64 << 20,
    call_depth: lash_kernel_vm::Bounds::DEFAULT_CALL_DEPTH,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[derive(Default)]
struct Console {
    lines: Vec<String>,
}

impl Host for Console {
    fn perform(
        &mut self,
        use_: &lash_kernel_value::OperationUse,
        _: &[Datum],
        _: &lash_kernel_value::sites::PositionLabel,
        _: &lash_kernel_value::sites::Site,
    ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, lash_kernel_vm::HostFault> {
        crate::tests::linked::read(use_, 0)
    }

    fn print(&mut self, value: &Datum) {
        self.lines.push(match value {
            Datum::Text(line) => line.clone(),
            other => format!("{other:?}"),
        });
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// One stretch of a run between deliveries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct Epoch {
    /// The outcomes delivered before the stretch ran, in order.
    pub(crate) delivered: Vec<String>,
    /// The tool calls and sleeps the stretch requested, in order.
    pub(crate) asked: Vec<String>,
    /// The lines the stretch printed.
    pub(crate) logged: Vec<String>,
}

/// A whole run: its epochs and how it ended, `ok` or the error.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Recorded {
    pub(crate) epochs: Vec<Epoch>,
    pub(crate) end: String,
}

impl Recorded {
    /// Every line the run printed, in order.
    pub(crate) fn lines(&self) -> Vec<&str> {
        self.epochs
            .iter()
            .flat_map(|epoch| epoch.logged.iter().map(String::as_str))
            .collect()
    }
}

fn shown(datum: Option<&Datum>) -> String {
    match datum {
        Some(Datum::Text(text)) => text.clone(),
        Some(Datum::Int(value)) => value.to_string(),
        other => format!("{other:?}"),
    }
}

/// What a request is called in a delivery script: `host.echo:a`, `host.boom:x`,
/// `sleep:50`.
fn label(request: &Request) -> String {
    match request {
        Request::Effect(effect) => {
            let name = crate::tests::linked::operation(&effect.use_);
            if name.as_str() == "python.sleep" {
                let Some(Datum::Int(milliseconds)) = effect.args.first() else {
                    panic!("sleep milliseconds")
                };
                format!("sleep:{milliseconds}")
            } else {
                format!("{name}:{}", shown(effect.args.first()))
            }
        }
    }
}

fn answer(request: &Request) -> (WaitId, Outcome) {
    match request {
        Request::Effect(effect) => {
            let argument =
                if crate::tests::linked::operation(&effect.use_).as_str() == "python.sleep" {
                    Datum::Null
                } else {
                    effect.args.first().cloned().unwrap_or(Datum::Null)
                };
            let outcome = if crate::tests::linked::operation(&effect.use_).as_str() == "host.boom" {
                Outcome::Failed(ErrorDatum {
                    kind: "boom".to_string(),
                    message: shown(Some(&argument)),
                    data: Datum::Null,
                })
            } else {
                Outcome::Completed(argument)
            };
            (effect.wait, outcome)
        }
    }
}

pub(super) fn ending(end: End) -> String {
    match end {
        End::Finished(_) => "ok".to_string(),
        End::Error(RunError::Uncaught {
            error: Datum::Error(error),
            ..
        }) => {
            format!("error {}: {}", error.kind, error.message)
        }
        End::Error(RunError::Uncaught { error: other, .. }) => format!("error {other:?}"),
        End::Error(RunError::TasksOutstanding {
            unfinished,
            unobserved,
            ..
        }) => format!(
            "tasks outstanding: {} unfinished, {} unobserved",
            unfinished.len(),
            unobserved.len()
        ),
        End::Error(error) => format!("run error: {error}"),
        End::Failed(reason) => format!("failed {reason:?}"),
        End::Cancelled => "cancelled".to_string(),
    }
}

/// Lowers `source` as a cell of a session that holds `bindings`, admits
/// the document and starts a machine on it. Gives the machine and the
/// document's kernel text.
fn start(
    source: &str,
    effects: BTreeMap<EffectName, Signature>,
    session: Session,
) -> (KernelMachine, String) {
    let names = session.names();
    let lowered = lower_with(source, &effects, &names).unwrap_or_else(|error| panic!("{error}"));
    let text = lash_kernel_doc::print_document(&lowered.document);
    let map = lowered
        .source_map
        .as_ref()
        .expect("a lowered cell has its source map");
    if let Err(refused) = map.validate(&lowered.document, source) {
        panic!("{refused}\n{text}");
    }
    let mut environment = lash_kernel_check::Environment::new(library());
    environment.bindings = names;
    if let Err(refusal) = lash_kernel_check::admit(&lowered.document, &environment) {
        panic!("{refusal:?}\n{text}");
    }
    let program = crate::tests::linked::program(
        Arc::clone(lowered.document.shared()),
        installed().prepared.clone(),
    );
    let machine = pages::start(program, BOUNDS, &session);
    (machine, text)
}

/// FIG-6160: a builtin range loop needs space for its counter, regardless
/// of the number of integers it could visit before a break.
#[test]
fn builtin_range_loop_uses_constant_space() {
    let lowered = lower("for i in range(100000):\n    print(i)\n    break\nprint(i)\n").unwrap();
    let program = crate::tests::linked::program(
        Arc::clone(lowered.document.shared()),
        installed().prepared.clone(),
    );
    let mut machine = pages::start(
        program,
        Bounds {
            memory: 64 << 10,
            ..BOUNDS
        },
        &Session::default(),
    );
    let mut console = Console::default();
    assert!(matches!(
        machine.run(&mut console, u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    assert_eq!(console.lines, ["0", "0"]);
}

/// FIG-6886: a key sort applies its key once per item, in order, and sorts
/// the keys natively: `sorted` and `list.sort`, reversed or not, order
/// stably by key and charge the same for any order of the same items.
#[test]
fn a_key_sort_computes_each_key_once_and_charges_alike_in_any_order() {
    const N: usize = 64;
    let orders: [Vec<usize>; 3] = [
        (0..N).collect(),
        (0..N).rev().collect(),
        (0..N).map(|i| i * 37 % N).collect(),
    ];
    let mut charges = Vec::new();
    for items in &orders {
        let listed = items
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!(
            "calls = []\ndef rank(x):\n    calls.append(x)\n    return x % 7\n\
             xs = [{listed}]\nys = sorted(xs, key=rank)\nxs.sort(key=rank, reverse=True)\n\
             print(calls == [{listed}, {listed}])\nprint(ys)\nprint(xs)\n"
        );
        let lowered = lower(&source).unwrap_or_else(|error| panic!("{error}"));
        let program = crate::tests::linked::program(
            Arc::clone(lowered.document.shared()),
            installed().prepared.clone(),
        );
        let observed = execution_observation(program, BOUNDS, false);
        assert!(
            matches!(observed.end, End::Finished(_)),
            "{:?}",
            observed.end
        );
        let mut ascending = items.clone();
        ascending.sort_by_key(|x| x % 7);
        let mut descending = items.clone();
        descending.sort_by_key(|x| std::cmp::Reverse(x % 7));
        let shown = |xs: &[usize]| {
            format!(
                "[{}]",
                xs.iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        assert_eq!(
            observed.lines,
            ["True".to_owned(), shown(&ascending), shown(&descending)]
        );
        charges.push(observed.charged);
    }
    assert!(
        charges.iter().all(|charge| *charge == charges[0]),
        "a key sort's charge depends on the order of its items: {charges:?}"
    );
}

/// FIG-6904 / K-CHG-007: sorting bills the actual callback-produced keys,
/// even when they are large captured values absent from the input/result.
/// Key construction and callback work stay fixed; only the native merge's
/// deep-key charge changes, equally for every input order and both APIs.
#[test]
fn a_key_sort_bills_large_captured_keys_exactly_in_any_order() {
    let mut charges = Vec::new();
    for items in ["0, 1, 2", "2, 1, 0", "1, 2, 0"] {
        let mut by_key_size = Vec::new();
        for keys in ["small", "large"] {
            let source = format!(
                "small = [[1], [1], [1]]\n\
                 large = [[1] * 10000, [1] * 10000, [1] * 10000]\n\
                 keys = {keys}\n\
                 def rank(x):\n    return keys[x]\n\
                 xs = [{items}]\nys = sorted(xs, key=rank)\n\
                 xs.sort(key=rank, reverse=True)\nprint(ys)\nprint(xs)\n"
            );
            let lowered = lower(&source).unwrap_or_else(|error| panic!("{error}"));
            let program = crate::tests::linked::program(
                Arc::clone(lowered.document.shared()),
                installed().prepared.clone(),
            );
            let observed = execution_observation(program, BOUNDS, false);
            assert!(
                matches!(observed.end, End::Finished(_)),
                "{:?}",
                observed.end
            );
            assert_eq!(observed.lines, [format!("[{items}]"), format!("[{items}]")]);
            by_key_size.push(observed.charged);
        }
        // Three distinct lists gain 9999 members each: one shallow slot
        // and two units for integer 1. The native bills twice this deep
        // size over 1 + ceil_log2(size(keys)) = 3 passes, in each sort.
        let deep_difference = 3 * 9999 * (1 + 2);
        assert_eq!(by_key_size[1] - by_key_size[0], 2 * 2 * deep_difference * 3);
        charges.push(by_key_size);
    }
    assert!(charges.iter().all(|charge| *charge == charges[0]));
}

/// FIG-6406: replacing every visited key preserves size but must exhaust
/// the dict iterator's entry budget instead of extending the loop forever.
#[test]
fn dict_same_size_key_replacement_raises_within_charge_bound() {
    // CPython table compaction happens to exhaust the three-key case.
    // The FIG-6406 ruling permits Lash's stricter detection there too.
    for (initial, visited) in [
        ("{'a': 1, 'b': 2}", vec!["a", "b"]),
        ("{'0': 1, '1': 1, '2': 1}", vec!["0", "1", "2"]),
    ] {
        let source =
            format!("d = {initial}\nfor k in d:\n    print(k)\n    del d[k]\n    d[k + 'x'] = 1\n");
        let lowered = lower(&source).unwrap();
        let program = crate::tests::linked::program(
            Arc::clone(lowered.document.shared()),
            installed().prepared.clone(),
        );
        let bounds = Bounds {
            charge_limit: 20_000,
            ..BOUNDS
        };
        let straight = execution_observation(program.clone(), bounds, false);
        assert_eq!(
            ending(straight.end.clone()),
            "error RuntimeError: dictionary keys changed during iteration"
        );
        assert_eq!(straight.lines, visited);
        // K-MACH-007/008: the remaining budget, size and live map cursor must
        // survive serialization at every safe point with identical charge.
        assert_eq!(execution_observation(program, bounds, true), straight);
    }
}

/// Runs `source` to its end as a cell of a session that holds `bindings`
/// and can call the tools `effects`. At each park, `at_park` is given the
/// lines printed since the park before, the park and the document's
/// kernel text, and answers the waits it chooses to. Gives the lines
/// printed after the last park and the run's end.
pub(crate) fn drive(
    source: &str,
    effects: BTreeMap<EffectName, Signature>,
    session: Session,
    mut at_park: impl FnMut(Vec<String>, Park, &str) -> Vec<(WaitId, Outcome)>,
) -> (Vec<String>, End) {
    let (mut machine, text) = start(source, effects, session);
    let mut console = Console::default();
    loop {
        let step = machine
            .run(&mut console, u64::MAX)
            .unwrap_or_else(|error| panic!("{error}\n{text}"));
        let lines = std::mem::take(&mut console.lines);
        match step {
            Step::Slice => unreachable!("the slice is unbounded"),
            Step::Ended(end) => return (lines, end),
            Step::Parked(park) => {
                for (wait, outcome) in at_park(lines, park, &text) {
                    machine
                        .deliver(wait, outcome)
                        .unwrap_or_else(|error| panic!("{error}"));
                }
            }
        }
    }
}

/// Lowers `source` as a first cell and runs it to its end. At each park the
/// next batch of `deliveries` is delivered, in the order it lists; once the
/// script is used up, the oldest pending request.
pub(crate) fn run(source: &str, deliveries: &[Vec<String>]) -> Recorded {
    run_cell(source, deliveries, Session::default()).0
}

/// Runs an entry with positional host arguments through the real machine.
pub(super) fn end_of_entry(document: lash_kernel_doc::Document, args: Vec<Datum>) -> Datum {
    entry_output(document, args).0
}

/// What an entry that calls no tool returns, and the lines it prints.
pub(super) fn entry_output(
    document: lash_kernel_doc::Document,
    args: Vec<Datum>,
) -> (Datum, Vec<String>) {
    let session = Session::default();
    let program = crate::tests::linked::program(Arc::new(document), installed().prepared.clone());
    let mut machine = KernelMachine::start(
        program,
        BOUNDS,
        lash_kernel_vm::CellStart {
            owner: pages::OWNER,
            target: lash_kernel_vm::Target::Entry(Name::new("run")),
            args,
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(session.source.clone()),
    )
    .unwrap();
    let mut console = Console::default();
    match machine.run(&mut console, u64::MAX).unwrap() {
        Step::Ended(End::Finished(finished)) => (finished.result, console.lines),
        other => panic!("{other:?}"),
    }
}

/// Start a copied callable in a fresh machine with its declared host ABI.
pub(super) fn callable_result(
    text: &str,
    function: lash_kernel_doc::FunctionValue,
    args: Vec<Datum>,
) -> Datum {
    let document = lash_kernel_doc::parse_document(text).expect("defining document");
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                panic!("law operation")
            };
            (
                *slot,
                lash_kernel_value::Fill::Operation(lash_kernel_value::OperationRequirement::new(
                    signature.clone(),
                    lash_kernel_value::ContractId::new("fixture"),
                )),
            )
        })
        .collect();
    let link = lash_kernel_value::Link {
        helper_fills: Default::default(),
        document: document.identity(),
        fills,
    };
    let content = lash_kernel_package::LinkedContent {
        documents: [(document.identity(), std::sync::Arc::new(document.clone()))].into(),
        links: [(link.identity(), link)].into(),
        ..Default::default()
    };
    let materialized = lash_kernel_package::materialize_workflow(
        &content,
        &lash_kernel_doc::WorkflowReference::Callable(function.callable),
        Some(&function),
        None,
        installed().prepared.helpers(),
    )
    .expect("materialized callable");
    let materialized_result = end_of_entry(materialized.document, args.clone());
    let mut program = super::linked::program(Arc::new(document), installed().prepared.clone());
    program.functions.insert(function.callable.function);
    let pages = pages::Source::default();
    let mut machine = KernelMachine::start(
        program.clone(),
        BOUNDS,
        lash_kernel_vm::CellStart {
            owner: pages::OWNER,
            target: lash_kernel_vm::Target::Callable(Box::new(function)),
            args,
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(pages.clone()),
    )
    .expect("fresh callable start");
    let result = loop {
        match machine.run(&mut Console::default(), 1).unwrap() {
            Step::Ended(End::Finished(finished)) => break finished.result,
            Step::Slice => {
                let checkpoint = machine.checkpoint().unwrap().checkpoint();
                machine = KernelMachine::open(
                    program.clone(),
                    BOUNDS,
                    &checkpoint,
                    Box::new(pages.clone()),
                )
                .expect("cold callable root");
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(materialized_result, result);
    result
}

/// The lines a cell that calls no tool prints, and how it ends.
pub(crate) fn output(source: &str) -> (Vec<String>, String) {
    let recorded = run(source, &[]);
    let lines = recorded.lines().into_iter().map(str::to_string).collect();
    (lines, recorded.end)
}

/// Runs the cells of one session in order, each starting with the
/// bindings the one before it left, and gives every line they printed.
pub(crate) fn session(cells: &[&str]) -> Vec<String> {
    let mut session = Session::default();
    let mut lines = Vec::new();
    for cell in cells {
        let (recorded, carried) = run_cell(cell, &[], session);
        assert_eq!(recorded.end, "ok", "{cell}");
        lines.extend(recorded.lines().into_iter().map(str::to_string));
        session = carried.unwrap_or_default();
    }
    lines
}

/// [`run`] in a session that holds `bindings`. Also gives the bindings a
/// cell that finished leaves to the next.
fn run_cell(
    source: &str,
    deliveries: &[Vec<String>],
    session: Session,
) -> (Recorded, Option<Session>) {
    let mut pending: Vec<Request> = Vec::new();
    let mut script = deliveries.iter();
    let mut epochs = Vec::new();
    let mut delivered = Vec::new();
    let (logged, end) = drive(source, effects(), session.clone(), |logged, park, text| {
        pending.retain(|request| {
            let wait = match request {
                Request::Effect(effect) => effect.wait,
            };
            !park.cancel_requests.contains(&wait)
        });
        epochs.push(Epoch {
            delivered: std::mem::take(&mut delivered),
            asked: park.requests.iter().map(label).collect(),
            logged,
        });
        pending.extend(park.requests);
        let batch = match script.next() {
            Some(batch) => batch.clone(),
            None => {
                vec![label(pending.first().unwrap_or_else(|| {
                    panic!("a park with nothing pending\n{text}")
                }))]
            }
        };
        let outcomes = batch
            .iter()
            .map(|name| {
                let index = pending
                    .iter()
                    .position(|request| label(request) == *name)
                    .unwrap_or_else(|| panic!("no pending request `{name}`\n{text}"));
                answer(&pending.remove(index))
            })
            .collect();
        delivered = batch;
        outcomes
    });
    assert!(script.next().is_none(), "unused delivery batches");
    epochs.push(Epoch {
        delivered,
        asked: Vec::new(),
        logged,
    });
    let carried = match &end {
        End::Finished(finished) => Some(Session {
            root: finished.carry,
            source: session.source.clone(),
        }),
        _ => None,
    };
    (
        Recorded {
            epochs,
            end: ending(end),
        },
        carried,
    )
}

// These previous definitions are held only by this law, as sealed releases
// hold old bodies. The unsealed production baseline keeps current bodies.
const PREVIOUS_ARITHMETIC: &str = r#"
use kind
use same
use add
use sub
use text.concat
use list.concat
use tuple.concat
use list.len
use set.contains
use bool.not
use py.is_number
use py.operand_error
use py.to_list
use py.add

function py.add(left: Any, right: Any) -> Any
kernel 1
charge sum(4, size(left), size(right))
body {
  let k = kind(left)
  if same(k, kind(right)) {
    if same(k, "text") {
      return text.concat(left, right)
    }
    if same(k, "list") {
      return list.concat(left, right)
    }
    if same(k, "tuple") {
      return tuple.concat(left, right)
    }
  }
  let ok = invoke py.is_number(left)
  if ok {
    set ok = invoke py.is_number(right)
  }
  if ok {
    return add(left, right)
  }
  do invoke py.operand_error("+", left, right)
  return null
}

function py.iadd(left: Any, right: Any) -> Any
kernel 1
charge sum(4, size(left), size(right))
body {
  if same(kind(left), "list") {
    let items = invoke py.to_list(right)
    for item in items {
      set left[list.len(left)] = item
    }
    return left
  }
  let out = invoke py.add(left, right)
  return out
}

function py.sub(left: Any, right: Any) -> Any
kernel 1
charge sum(4, size(left), size(right))
body {
  let ok = invoke py.is_number(left)
  if ok {
    set ok = invoke py.is_number(right)
  }
  if ok {
    return sub(left, right)
  }
  if same(kind(left), "set") {
    if same(kind(right), "set") {
      let out = set{}
      for member in left {
        if bool.not(set.contains(right, member)) {
          set out[member] = true
        }
      }
      return out
    }
  }
  do invoke py.operand_error("-", left, right)
  return null
}
"#;

const ARITHMETIC_RESUME: &str = "\
n = await host.echo(7)
x = n + 3
y = n - 3
n += 3
print(n, x, y)
big = 9223372036854775807
big += 1
print(big, big + 1, big - 1)
x = 1.5
x += 2
print(x, 2 + 1.5, 1.5 + 2, 2 - 1.5, 1.5 - 2)
a = [1]
alias = a
a += a
a += (3,)
print(alias, alias is a)
print('a' + 'b', (1,) + (2,), [1] + [2], {1, 2} - {2})
t = 'a'
t += 'b'
u = (1,)
u += (2,)
print(t, u)
try:
    True + 1
except TypeError as e:
    print(e)
try:
    1 - 'x'
except TypeError as e:
    print(e)
try:
    a += 1
except TypeError as e:
    print(e)
";

/// Carry equality is the reachable graph, including aliases and cycles;
/// projection ids, garbage and the conservative census are storage details.
#[derive(Debug, PartialEq)]
pub(super) struct ExecutionObservation {
    pub(super) end: End,
    pub(super) lines: Vec<String>,
    requests: Vec<String>,
    charged: u64,
    carry: Option<Checkpoint>,
}

pub(super) fn execution_observation(
    program: Program,
    bounds: Bounds,
    rebuild: bool,
) -> ExecutionObservation {
    execution_observation_with_layout(program, bounds, rebuild, Default::default())
}

fn execution_observation_with_layout(
    program: Program,
    bounds: Bounds,
    rebuild: bool,
    layout: lash_kernel_vm::testing::ExecutionStrategy,
) -> ExecutionObservation {
    execution_observation_with_memory(program, bounds, rebuild, layout).0
}

fn execution_observation_with_memory(
    program: Program,
    bounds: Bounds,
    rebuild: bool,
    layout: lash_kernel_vm::testing::ExecutionStrategy,
) -> (ExecutionObservation, u64) {
    execution_observation_with_meter(program, bounds, rebuild, layout, &mut |_| {})
}

fn execution_observation_with_meter(
    program: Program,
    bounds: Bounds,
    rebuild: bool,
    layout: lash_kernel_vm::testing::ExecutionStrategy,
    on_park: &mut dyn FnMut(u64),
) -> (ExecutionObservation, u64) {
    execution_observation_with_stops(program, bounds, rebuild, layout, on_park, &mut |_, _| {})
}

fn execution_observation_with_stops(
    program: Program,
    bounds: Bounds,
    rebuild: bool,
    layout: lash_kernel_vm::testing::ExecutionStrategy,
    on_park: &mut dyn FnMut(u64),
    on_stop: &mut dyn FnMut(&Step, &mut KernelMachine),
) -> (ExecutionObservation, u64) {
    let session = Session::default();
    let mut machine = lash_kernel_vm::testing::start(
        program.clone(),
        bounds,
        lash_kernel_vm::CellStart {
            owner: pages::OWNER,
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: session.root,
            modes: Default::default(),
            host: Vec::new(),
        },
        layout,
        Box::new(session.source.clone()),
    )
    .unwrap();
    let mut host = Console::default();
    let mut requests = Vec::new();
    loop {
        let step = machine
            .run(&mut host, if rebuild { 1 } else { u64::MAX })
            .unwrap_or_else(|error| panic!("the document runs in {layout:?}, rebuild={rebuild}, bounds={bounds:?}, charged={:?}: {error:?}", machine.meters().charged));
        on_stop(&step, &mut machine);
        if let Step::Ended(mut end) = step {
            let carry = match &mut end {
                End::Finished(finished) => finished.carry.take().map(|root| {
                    semantic::session(&root, Box::new(session.source.clone()))
                        .expect("the carried graph is readable")
                }),
                _ => None,
            };
            return (
                ExecutionObservation {
                    end,
                    lines: host.lines,
                    requests,
                    charged: machine.meters().charged.expect("exact charge"),
                    carry,
                },
                machine.meters().memory,
            );
        }
        if rebuild {
            let sealed = machine.checkpoint().unwrap();
            machine = lash_kernel_vm::testing::open(
                program.clone(),
                bounds,
                &sealed.checkpoint(),
                Box::new(session.source.clone()),
                layout,
            )
            .unwrap();
        }
        if let Step::Parked(park) = step {
            on_park(machine.meters().memory);
            for request in park.requests {
                requests.push(label(&request));
                let (wait, outcome) = answer(&request);
                machine
                    .deliver(wait, outcome)
                    .expect("the echo outcome is delivered");
            }
        }
    }
}

/// K-MACH-007/008, K-CHG-007: new arithmetic identities preserve each
/// document's result, effects, aliases and exact formula charge across slices.
/// Previous definitions held beside them still resolve their original pins.
#[test]
fn paged_arithmetic_keeps_held_definitions_and_exact_charge() {
    use lash_kernel_dialect::Library;
    let current = installed().prepared.registry();
    let names = ["py.add", "py.iadd", "py.sub"];
    let ids: BTreeSet<_> = names
        .iter()
        .map(|name| library().resolve(name).unwrap())
        .collect();
    let roots = lash_kernel_releases::helper_release()
        .expect("helper release")
        .resolver_roots();
    let mut previous =
        NamedLibrary::resolving(current, |id| roots.contains(id) && !ids.contains(id)).unwrap();
    let definitions =
        lash_kernel_dialect::define_functions(PREVIOUS_ARITHMETIC, &mut previous, &[])
            .expect("the previous arithmetic definitions resolve");
    let mut registry = current.as_ref().clone();
    for definition in definitions {
        assert!(
            !ids.contains(&definition.identity()),
            "a successor has a new identity"
        );
        registry
            .register(definition, None)
            .expect("previous and current bodies coexist");
    }
    let prepared = PreparedLibrary::with_helpers(
        Arc::new(registry.clone()),
        Arc::new(
            lash_kernel_doc::BuildHelpers::new(
                registry.clone(),
                registry.iter().map(|(id, _)| *id),
            )
            .expect("closed helpers"),
        ),
    );
    let effects = effects();
    let mut observations = Vec::new();
    for named in [library(), &previous] {
        let lowered = crate::lower(
            ARITHMETIC_RESUME,
            &Environment::new(
                named,
                &crate::tests::linked::namespaces(&effects, &Default::default())
                    .expect("the test's effects are operations"),
            ),
            lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        )
        .expect("the arithmetic witness lowers");
        let program =
            crate::tests::linked::program(Arc::clone(lowered.document.shared()), prepared.clone());
        let straight = execution_observation(program.clone(), BOUNDS, false);
        assert!(
            matches!(straight.end, End::Finished(_)),
            "{:?}",
            straight.end
        );
        assert_eq!(straight.requests, ["host.echo:7"]);
        assert_eq!(
            execution_observation(
                program,
                Bounds {
                    charge_limit: straight.charged,
                    ..BOUNDS
                },
                true
            ),
            straight
        );
        observations.push(straight);
    }
    assert_eq!(
        observations[0].end, observations[1].end,
        "old and new documents finish identically"
    );
    assert_eq!(observations[0].carry, observations[1].carry);
    assert_eq!(
        observations[0].lines, observations[1].lines,
        "numeric and fallback observations agree"
    );
    assert_eq!(
        observations[0].charged, observations[1].charged,
        "held and current helpers declare the same charge formulas"
    );
}

/// FIG-6115: a nested function may change a captured binding after its
/// integer initialization; the enclosing loop must retain dynamic dispatch.
#[test]
fn integer_facts_exclude_captured_and_nonlocal_bindings() {
    let source = r#"
def outer():
    x = 1
    def change():
        nonlocal x
        x = 'a'
    for i in range(2):
        if i == 0:
            change()
        x += 'b'
    print(x)
outer()
"#;
    assert_eq!(output(source), (vec!["abb".into()], "ok".into()));
    assert!(calls_helper(source, "py.iadd"));
    for read_only in [
        "x = 1\ndef read():\n    return x\nfor i in range(2):\n    x += i\nprint(read())",
        "x = 1\nread = lambda: x\nfor i in range(2):\n    x += i\nprint(read())",
    ] {
        assert_eq!(output(read_only), (vec!["2".into()], "ok".into()));
        assert!(calls_helper(read_only, "py.iadd"));
    }
}

/// FIG-6115: any non-integer definition reaching a loop use prevents an
/// integer fact, including a string/float at a later iteration's backedge.
#[test]
fn integer_facts_exclude_string_and_float_reassignments() {
    for (source, expected, helper) in [
        (
            "x = 1\nfor i in range(2):\n    x = 'a'\n    x += 'b'\nprint(x)",
            "ab",
            "py.iadd",
        ),
        (
            "x = 1\nfor i in range(2):\n    x += 1\n    x = 1.5\nprint(x)",
            "1.5",
            "py.number.iadd",
        ),
        (
            "x = 1\nif True:\n    x = 1.5\nx += 1\nprint(x)",
            "2.5",
            "py.number.iadd",
        ),
    ] {
        assert_eq!(output(source), (vec![expected.into()], "ok".into()));
        assert!(calls_helper(source, helper));
    }
}

/// FIG-6115: a definition on only one branch cannot bypass the Python
/// unbound-local check at a merged use, even if every definition is int.
#[test]
fn integer_facts_check_unbound_paths() {
    let source = r#"
def f(flag):
    if flag:
        x = 1
    try:
        x += 1
        print(x)
    except UnboundLocalError:
        print('unbound')
f(False)
f(True)
"#;
    assert_eq!(
        output(source),
        (vec!["unbound".into(), "2".into()], "ok".into())
    );
    assert!(calls_helper(source, "py.iadd"));
}

/// FIG-6115: Python implicitly deletes an except target; the next read
/// raises its unbound-local error.
#[test]
fn integer_facts_exclude_deleted_bindings() {
    let source = r#"
def f():
    x = 1
    try:
        raise ValueError()
    except ValueError as x:
        pass
    try:
        x += 1
    except UnboundLocalError:
        print('deleted')
    x = 1
    x += 1
    print(x)
f()
"#;
    assert_eq!(
        output(source),
        (vec!["deleted".into(), "2".into()], "ok".into())
    );
    assert!(calls_helper(source, "py.iadd"));
}

/// FIG-6115: global declarations allow a call to replace an integer with
/// another type; facts cannot cross the scope/call boundary.
#[test]
fn integer_facts_exclude_global_bindings() {
    let source = r#"
x = 1
def change():
    global x
    x = 1.5
change()
x += 1
print(x)
"#;
    assert_eq!(output(source), (vec!["2.5".into()], "ok".into()));
    assert!(calls_helper(source, "py.iadd"));
}

/// FIG-6115: supported bool-to-int conversion remains guarded; integer
/// facts preserve signed floor division/modulo and negative-power results.
#[test]
fn integer_facts_preserve_boolean_and_signed_arithmetic() {
    let source = r#"
x = -7
for i in range(2):
    x += i
print(x // 4, x % 4)
x = int(True)
for i in range(2):
    x += i
print(x)
x = int(False)
for i in range(2):
    x += i
print(x)
x = 2 ** -1
x += 1
print(x)
x = True
try:
    x += 1
except TypeError:
    print('bool remains guarded')
"#;
    assert_eq!(
        output(source),
        (
            vec![
                "-2 2".into(),
                "2".into(),
                "1".into(),
                "1.5".into(),
                "bool remains guarded".into()
            ],
            "ok".into()
        )
    );
}

/// FIG-6115, K-MACH-007/008, K-CHG-007: serialization at every safe point
/// (including an await in the loop) restores the exact facts' values,
/// requests, result and charge; no compiler fact becomes runtime state.
#[test]
fn paged_integer_loop_keeps_facts_across_await() {
    let source = r#"
s = 9223372036854775807
start = 0
stop = 3
step = 1
for i in range(start, stop, step):
    s += i
    if i == 1:
        await host.echo(s)
print(s)
"#;
    assert!(!calls_helper(source, "py.iadd"));
    let lowered = lower(source).expect("the integer loop lowers");
    let program = crate::tests::linked::program(
        Arc::clone(lowered.document.shared()),
        installed().prepared.clone(),
    );
    let straight = execution_observation(program.clone(), BOUNDS, false);
    assert!(matches!(straight.end, End::Finished(_)));
    assert_eq!(straight.lines, ["9223372036854775810"]);
    assert_eq!(straight.requests, ["host.echo:9223372036854775808"]);
    assert_eq!(
        execution_observation(
            program,
            Bounds {
                charge_limit: straight.charged,
                ..BOUNDS
            },
            true
        ),
        straight
    );
}

/// FIG-6115: a shadowed range is an arbitrary callable, and a restored
/// binding is an unknown reaching definition, even in an integer recurrence.
#[test]
fn integer_facts_exclude_shadowed_ranges_and_restored_values() {
    let source = r#"
def f(range):
    for x in range(2):
        x += 1
        print(x)
f(lambda n: [1.5])
"#;
    assert_eq!(output(source), (vec!["2.5".into()], "ok".into()));
    assert!(calls_helper(source, "py.iadd"));
    assert_eq!(
        session(&["x = 1.5", "for i in range(2):\n    x += 1\nprint(x)"]),
        ["3.5"]
    );
}

/// K-LTXT-012: Python materializes the iterable before admission, names the
/// first mismatch, and never coerces a member to str.
#[test]
fn join_admission_keeps_materialization_and_indexed_type_errors() {
    let (lines, end) = output(
        r#"
print('|'.join(['', '😀', 'é', '']))
print('|'.join(('a', 'b')), '-'.join('ab'), ','.join([]))
for values in [[None, 7], ['a', 7, None], ['a', 'b', None], ['a', []]]:
    try:
        print('|'.join(values))
    except TypeError as error:
        print(error)
try:
    print('|'.join(7))
except TypeError as error:
    print(error)
"#,
    );
    assert_eq!(end, "ok");
    assert_eq!(
        lines,
        [
            "|😀|é|",
            "a|b a-b ",
            "sequence item 0: expected str instance, NoneType found",
            "sequence item 1: expected str instance, int found",
            "sequence item 2: expected str instance, NoneType found",
            "sequence item 1: expected str instance, list found",
            "'int' object is not iterable"
        ]
    );
}

/// FIG-6160: compare observable values, errors and final loop targets with
/// the still-supported eager range, forced by wrapping it in list().
fn range_matches_eager(source: &str, args: &str, lines: &[&str]) {
    let counted = source.replace("ITER", &format!("range({args})"));
    let eager = source.replace("ITER", &format!("list(range({args}))"));
    let actual = output(&counted);
    assert_eq!(actual, output(&eager), "{counted}");
    assert_eq!(
        actual,
        (
            lines.iter().map(|line| (*line).into()).collect(),
            "ok".into()
        ),
        "{counted}"
    );
}

/// FIG-6160: arguments run once, left to right, before validation; their
/// captured values cannot be changed by later arguments or by the body.
#[test]
fn range_arguments_are_evaluated_once_in_order() {
    range_matches_eager(
        r#"
trace = []
first = 1
stop = 6
step = 2
def end():
    global first
    first = 99
    trace.append('stop')
    return stop
def stride():
    trace.append('step')
    return step
for i in ITER:
    print(i)
    stop = 0
    step = 0
print(trace, first, i)
"#,
        "first, end(), stride()",
        &["1", "3", "5", "['stop', 'step'] 99 5"],
    );
    range_matches_eager(
        r#"
def argument(label, value):
    print(label)
    return value
try:
    for i in ITER:
        print('body')
except TypeError as error:
    print(type(error).__name__, error)
"#,
        "argument('start', 'bad'), argument('stop', 0), argument('step', 1)",
        &[
            "start",
            "stop",
            "step",
            "TypeError 'str' object cannot be interpreted as an integer",
        ],
    );
    let source = "for i in ITER:\n    print(i)\n    await host.echo(i)\nprint(i)\n";
    let args = "await host.echo(1), await host.echo(4), await host.echo(1)";
    let counted = run(&source.replace("ITER", &format!("range({args})")), &[]);
    let eager = run(
        &source.replace("ITER", &format!("list(range({args}))")),
        &[],
    );
    assert_eq!(counted, eager, "argument and body parks retain their order");
    assert_eq!(counted.lines(), ["1", "2", "3", "3"]);
    assert_eq!(counted.end, "ok");
}

/// FIG-6160: a zero step raises before any iteration, including an empty
/// range, and does not assign the target or execute else.
#[test]
fn range_zero_step_raises_before_iteration() {
    for args in ["0, 3, 0", "3, 0, 0", "0, 0, 0"] {
        range_matches_eager(
            r#"
i = 42
try:
    for i in ITER:
        print('body')
    else:
        print('else')
except ValueError as error:
    print(type(error).__name__, error, i)
"#,
            args,
            &["ValueError range() arg 3 must not be zero 42"],
        );
    }
}

/// FIG-6160: either step direction visits exactly the eager sequence;
/// integer counters remain exact across the small-integer boundary.
#[test]
fn range_steps_preserve_integer_sequences() {
    let source = "values = []\nfor i in ITER:\n    values.append(i)\nprint(values)\n";
    for (args, line) in [
        ("5", "[0, 1, 2, 3, 4]"),
        ("1, 6, 2", "[1, 3, 5]"),
        ("5, -2, -2", "[5, 3, 1, -1]"),
        ("0, 5, -1", "[]"),
        ("5, 0, 1", "[]"),
        (
            "9223372036854775806, 9223372036854775810",
            "[9223372036854775806, 9223372036854775807, 9223372036854775808, 9223372036854775809]",
        ),
        (
            "-9223372036854775807, -9223372036854775811, -1",
            "[-9223372036854775807, -9223372036854775808, -9223372036854775809, -9223372036854775810]",
        ),
    ] {
        range_matches_eager(source, args, &[line]);
    }
}

/// FIG-6160: the iterator is independent of target rebinding, including
/// assignments made by a closure and a subscript target.
#[test]
fn range_counter_is_independent_of_target_rebinding() {
    range_matches_eager(
        r#"
for i in ITER:
    print(i)
    i = 'changed'
print(i)
"#,
        "3",
        &["0", "1", "2", "changed"],
    );
    range_matches_eager(
        r#"
i = 99
def change():
    global i
    i = 100
for i in ITER:
    print(i)
    change()
print(i)
"#,
        "3",
        &["0", "1", "2", "100"],
    );
    range_matches_eager(
        r#"
box = [99]
for box[0] in ITER:
    print(box[0])
    box[0] = 'changed'
print(box)
"#,
        "3",
        &["0", "1", "2", "['changed']"],
    );
}

/// FIG-6160: a loop leaves the last target binding; an empty loop leaves
/// an existing binding intact and an absent local unbound.
#[test]
fn range_target_keeps_its_last_value_or_stays_unbound() {
    range_matches_eager("for i in ITER:\n    pass\nprint(i)\n", "3", &["2"]);
    range_matches_eager("i = 99\nfor i in ITER:\n    pass\nprint(i)\n", "0", &["99"]);
    range_matches_eager(
        r#"
for i in ITER:
    pass
try:
    print(i)
except NameError:
    print('unbound')
"#,
        "0",
        &["unbound"],
    );
    range_matches_eager(
        r#"
def empty():
    for i in ITER:
        pass
    try:
        print(i)
    except UnboundLocalError:
        print('unbound')
empty()
"#,
        "0",
        &["unbound"],
    );
}

/// FIG-6160: continue advances even through finally; break skips else;
/// natural exhaustion (including zero iterations) runs else. Nested loops
/// retain their own break and continue targets.
#[test]
fn range_break_continue_and_else_keep_python_control_flow() {
    range_matches_eager(
        r#"
for i in ITER:
    try:
        if i == 1:
            continue
        if i == 3:
            break
        print('body', i)
    finally:
        print('finally', i)
else:
    print('else')
print(i)
"#,
        "5",
        &[
            "body 0",
            "finally 0",
            "finally 1",
            "body 2",
            "finally 2",
            "finally 3",
            "3",
        ],
    );
    range_matches_eager(
        r#"
for i in ITER:
    for j in range(2):
        if j == 0:
            continue
        print(i, j)
        break
    continue
else:
    print('else')
"#,
        "2",
        &["0 1", "1 1", "else"],
    );
    range_matches_eager(
        "for i in ITER:\n    pass\nelse:\n    print('else')\n",
        "0",
        &["else"],
    );
}

/// FIG-6160: non-integers retain py.range's existing typed errors and
/// validation order, even when the bounds imply no iterations or step zero.
#[test]
fn range_non_integer_errors_match_the_eager_helper() {
    let source = r#"
try:
    for i in ITER:
        print('body')
except TypeError as error:
    print(type(error).__name__, error)
"#;
    for (args, ty) in [
        ("1.0", "float"),
        ("'bad', 1.0, 0", "str"),
        ("0, 1.0, 'bad'", "float"),
        ("0, 0, []", "list"),
        ("True", "bool"),
        ("None", "NoneType"),
        ("0, 0, False", "bool"),
    ] {
        range_matches_eager(
            source,
            args,
            &[&format!(
                "TypeError '{ty}' object cannot be interpreted as an integer"
            )],
        );
    }
}

/// FIG-6160: only a builtin direct loop gets the counter. Lexically
/// shadowed and rebound ranges remain calls of those bindings;
/// stored ranges, list(range(...)) and len(range(...)) stay eager lists.
#[test]
fn range_lowering_respects_scope_and_other_range_uses() {
    assert_eq!(
        output(
            r#"
def local():
    def range(n):
        return ['custom', n]
    for i in range(2):
        print(i)
    range = lambda n: ['rebound', n]
    for i in range(3):
        print(i)
local()
"#
        ),
        (
            vec!["custom".into(), "2".into(), "rebound".into(), "3".into()],
            "ok".into()
        )
    );
    assert_eq!(
        output(
            r#"
def f(range):
    for i in range(2):
        print(i)
def custom(n):
    return ['parameter', n]
f(custom)
"#
        ),
        (vec!["parameter".into(), "2".into()], "ok".into())
    );
    assert_eq!(
        output(
            r#"
def outer():
    def range(n):
        return ['closure', n]
    def inner():
        for i in range(2):
            print(i)
    inner()
outer()
"#
        ),
        (vec!["closure".into(), "2".into()], "ok".into())
    );
    assert_eq!(
        output(
            r#"
stored = range(3)
print(stored, list(range(3)), len(range(3)), type(stored).__name__)
for i in stored:
    print(i)
"#
        ),
        (
            vec![
                "[0, 1, 2] [0, 1, 2] 3 list".into(),
                "0".into(),
                "1".into(),
                "2".into()
            ],
            "ok".into()
        )
    );
}

/// AWAIT-PHASE, `K-TASK-026`: Python's await policy adds no turn. A task
/// that awaits a tool in place and one that awaits a task awaiting a tool,
/// their outcomes delivered together in either order, continue as asyncio
/// runs them: the in-place awaiter first, since the awaited task must end
/// before its awaiter wakes. The lowered document takes no `then yield`.
#[test]
fn python_awaits_add_no_turn() {
    let source = "import asyncio\n\
        async def in_place():\n    v = await host.echo('i')\n    print(v)\n\
        async def inner():\n    return await host.echo('p')\n\
        async def promised():\n    t = asyncio.create_task(inner())\n    v = await t\n    print(v)\n\
        a = asyncio.create_task(in_place())\n\
        b = asyncio.create_task(promised())\n\
        await asyncio.gather(a, b)\n";
    let text = kernel_text(source);
    assert!(!text.contains("then yield"), "{text}");
    let lowered = lower(source).unwrap();
    let function = super::linked::helper_reference(&lowered.document, "py.await")
        .expect("await policy library reference");
    assert!(lowered.document.library.contains(&function));
    assert!(text.contains("via py.await"), "{text}");
    for order in [
        ["host.echo:i", "host.echo:p"],
        ["host.echo:p", "host.echo:i"],
    ] {
        let recorded = run(source, &[order.map(String::from).to_vec()]);
        assert_eq!(recorded.lines(), ["i", "p"], "{order:?}");
        assert_eq!(recorded.end, "ok");
    }
}

/// FIG-6960 PB: closed list/text locals use kernel collection and text
/// operations, with Python's result and argument-effect order intact.
#[test]
fn sequence_facts_lower_kernel_operations_and_preserve_effects() {
    let source = r#"
xs = []
alias = xs
text = ''
for n in range(3):
    part = 'row-' + str(n)
    text = text + part
    xs.append(part)
print(text, '|'.join(xs))
print(xs.append(alias.pop()))
xs.append(await host.echo('last'))
print(xs)
"#;
    let text = kernel_text(source);
    assert!(!calls_helper(source, "py.method.append"), "{text}");
    assert!(!calls_helper(source, "py.add"), "{text}");
    assert!(!calls_helper(source, "py.str"), "{text}");
    assert!(text.contains("num.to_text"), "{text}");
    assert!(text.contains("text.concat"), "{text}");
    let direct = run(source, &[]);
    let helpers = r#"
def append_helper(receiver, value):
    return receiver.append(value)
def add_helper(left, right):
    return left + right
def str_helper(value):
    return str(value)
"#;
    let fallback = source
        .replace("'row-' + str(n)", "add_helper('row-', str_helper(n))")
        .replace("text + part", "add_helper(text, part)")
        .replace("xs.append(", "append_helper(xs, ");
    let fallback = run(&format!("{helpers}\n{fallback}"), &[]);
    assert_eq!(direct.lines(), fallback.lines());
    assert_eq!(direct.end, fallback.end);
    assert_eq!(
        direct.epochs.iter().map(|e| &e.asked).collect::<Vec<_>>(),
        fallback.epochs.iter().map(|e| &e.asked).collect::<Vec<_>>()
    );
}

/// FIG-6960 PB: one incompatible definition anywhere, a capture, or an
/// unbound read prevents a sequence fact. Existing helper errors survive.
#[test]
fn sequence_facts_reject_incompatible_or_unbound_definitions() {
    for (source, helper, expected) in [
        (
            "x = []\nx.append(1)\nx = 1\ntry:\n    x.append(2)\nexcept AttributeError:\n    print('guarded')",
            "py.method.append",
            "guarded",
        ),
        (
            "x = ''\nx = x + 'a'\nx = []\ntry:\n    x = x + 'b'\nexcept TypeError:\n    print('guarded')",
            "py.add",
            "guarded",
        ),
        (
            "def f(flag):\n    if flag:\n        x = []\n    try:\n        x.append(1)\n    except UnboundLocalError:\n        print('unbound')\nf(False)",
            "py.method.append",
            "unbound",
        ),
        (
            "def f(flag):\n    if flag:\n        x = ''\n    try:\n        x = x + 'a'\n    except UnboundLocalError:\n        print('unbound')\nf(False)",
            "py.add",
            "unbound",
        ),
        (
            "x = []\ndef change():\n    global x\n    x = 1\nchange()\ntry:\n    x.append(1)\nexcept AttributeError:\n    print('guarded')",
            "py.method.append",
            "guarded",
        ),
        (
            "def outer():\n    x = ''\n    def change():\n        nonlocal x\n        x = []\n    change()\n    try:\n        x = x + 'a'\n    except TypeError:\n        print('guarded')\nouter()",
            "py.add",
            "guarded",
        ),
        (
            "def f():\n    def str(value):\n        return []\n    x = str(1)\n    x.append(2)\n    print(x)\nf()",
            "py.method.append",
            "[2]",
        ),
        (
            "x = []\ny = x\nx = 1\ny.append(2)\nprint(y)",
            "py.method.append",
            "[2]",
        ),
    ] {
        assert!(calls_helper(source, helper), "{source}");
        assert_eq!(
            output(source),
            (vec![expected.into()], "ok".into()),
            "{source}"
        );
    }
}

/// FIG-6960 PB, PLAN-INDEP: the direct operations preserve results,
/// effects and exact charge against the bodies' layout, across slices and
/// every bound. Member cases also compare metered held bytes.
#[test]
fn sequence_facts_agree_across_layouts_and_charge_bounds() {
    let started = std::time::Instant::now();
    whole_path_packets_agree();
    println!(
        "whole Python packet checkpoint comparisons completed at {:?}",
        started.elapsed()
    );
    use lash_kernel_vm::testing::{ExecutionStrategy, library};
    let source = r#"
import json
async def build():
    objects = [(), (3,), [4]]
    print(objects[0], objects[1], objects[2])
    objects[0] = (await host.echo(5),)
    print(objects[0])
    try:
        print(objects[3])
    except IndexError:
        print('bounds')
    parts = []
    text = ''
    floating = 0.5
    numbers = [1, 2, 3]
    total = 0
    for n in range(3):
        total += numbers[n]
        floating += 0.25 * floating
        part = 'λ' + str(n)
        text += part
        parts.append(part)
    parts.append(await host.echo(text))
    d = {'a': 1}
    d['a'] += 2
    print(d['a'], parts[0], floating, total)
    encoded = json.dumps({'z': [floating, -0.0], 'a': 'é'}, separators=(',', ':'))
    print(await host.echo(f"j{encoded}:{len(numbers)}"))
    parts[0] = text
    print(parts)
await build()
"#;
    let lowered = lower(source).unwrap();
    let program = |layout| {
        crate::tests::linked::program(
            Arc::clone(lowered.document.shared()),
            library(&installed().prepared, layout),
        )
    };
    let on = program(ExecutionStrategy::Optimized);
    let bodies = program(ExecutionStrategy::Interpreted);
    let mut reference_held = Vec::new();
    let reference = execution_observation_with_meter(
        bodies.clone(),
        BOUNDS,
        false,
        ExecutionStrategy::Interpreted,
        &mut |bytes| reference_held.push(bytes),
    )
    .0;
    let mut optimized_held = Vec::new();
    println!(
        "broad Python sequence reference charge: {} at {:?}",
        reference.charged,
        started.elapsed()
    );
    assert_eq!(
        execution_observation_with_meter(
            on.clone(),
            BOUNDS,
            false,
            ExecutionStrategy::Optimized,
            &mut |bytes| optimized_held.push(bytes)
        )
        .0,
        reference
    );
    assert_eq!(
        optimized_held, reference_held,
        "retained bytes at each park"
    );
    assert_eq!(
        execution_observation_with_layout(on.clone(), BOUNDS, true, ExecutionStrategy::Optimized),
        reference
    );
    // FIG-7513: terminal held bytes must not depend on whether an earlier
    // memory-pressure sweep already collected unreachable cycles.
    assert_eq!(
        execution_observation_with_memory(
            on.clone(),
            Bounds {
                memory: 43_520,
                ..BOUNDS
            },
            false,
            ExecutionStrategy::Optimized,
        ),
        execution_observation_with_memory(on.clone(), BOUNDS, false, ExecutionStrategy::Optimized,),
        "completed guest state is independent of earlier memory pressure",
    );
    // FIG-7513 / K-CHG-002: a construction bailout must retain every
    // unpaid expression event, including adjacent split region heads.
    // The bound reaches the repr closure after its nested text.join calls.
    let bounds = Bounds {
        memory: 38_912,
        ..BOUNDS
    };
    assert_eq!(
        execution_observation_with_memory(on.clone(), bounds, false, ExecutionStrategy::Optimized),
        execution_observation_with_memory(
            bodies.clone(),
            bounds,
            false,
            ExecutionStrategy::Interpreted
        ),
        "a memory refusal keeps the same completed charge prefix",
    );
    for charge_limit in 0..=reference.charged {
        if charge_limit.is_multiple_of(2048) {
            println!(
                "broad Python sequence charge bound {charge_limit} at {:?}",
                started.elapsed()
            );
        }
        let bounds = Bounds {
            charge_limit,
            ..BOUNDS
        };
        assert_eq!(
            execution_observation_with_layout(
                on.clone(),
                bounds,
                false,
                ExecutionStrategy::Optimized
            ),
            execution_observation_with_layout(
                bodies.clone(),
                bounds,
                false,
                ExecutionStrategy::Interpreted
            ),
            "bound {charge_limit}"
        );
    }

    for source in [
        "values = [(), (1,), [2]]\nprint(values[0], values[1], values[2])\n",
        "data = {'key': (1,), 'other': [2]}\nalias = data\nfirst = data['key']\nalias['key'] = (3,)\nprint(first, data['key'])\n",
        "values = []\nvalues.append(())\nvalues.append({'key': 1})\nalias = values\nalias.append([2])\nprint(values)\n",
        "data = {'key': [()]}\nprint(data['key'][0])\nprint(await host.echo(data['key']))\nprint(data['key'][0])\n",
    ] {
        let lowered = lower(source).unwrap();
        let program = |layout| {
            crate::tests::linked::program(
                Arc::clone(lowered.document.shared()),
                library(&installed().prepared, layout),
            )
        };
        for rebuild in [false, true] {
            assert_eq!(
                execution_observation_with_memory(
                    program(ExecutionStrategy::Optimized),
                    BOUNDS,
                    rebuild,
                    ExecutionStrategy::Optimized
                ),
                execution_observation_with_memory(
                    program(ExecutionStrategy::Interpreted),
                    BOUNDS,
                    rebuild,
                    ExecutionStrategy::Interpreted
                ),
                "member result, charge and metered held bytes, rebuild {rebuild}: {source}"
            );
        }
    }
}

// K-CHG-001 / CONT-EXACT / PLAN-INDEP: the actual dialect's complete
// read/update and allocating format paths preserve every observed boundary.
fn whole_path_packets_agree() {
    use lash_kernel_vm::testing::{self, ExecutionStrategy as Mode, Family, FamilyExit as Exit};
    let cases = [
        (
            Family::ReadUpdate,
            "def run(obj):\n    total = 1\n    total += obj['a']\n    return total\nprint(await host.echo(run({'a': 255})))\n",
        ),
        (
            Family::FormatAppend,
            "def build(n):\n    text = ''\n    for i in range(n):\n        text += f'Tool {i} value={i * 3}; status=ready\\n'\n    return text\nprint(await host.echo(build(3)))\n",
        ),
    ];
    let cases = cases.map(|(family, source)| {
        let lowered = lower(source).expect("the whole Python path lowers");
        let program = crate::tests::linked::program(
            Arc::clone(lowered.document.shared()),
            installed().prepared.clone(),
        );
        (family, source, program)
    });
    let observe = |program: &Program, bounds, rebuild, mode| {
        testing::begin_family_witnesses();
        let mut stops = Vec::new();
        let mut cold = Vec::new();
        let mut packets = (0, 0);
        let outcome = execution_observation_with_stops(
            program.clone(),
            bounds,
            rebuild,
            mode,
            &mut |_| {},
            &mut |step, machine| {
                let counts = testing::take_packet_counts(machine);
                packets.0 += counts.0;
                packets.1 += counts.1;
                stops.push((
                    match step {
                        Step::Ended(_) => "ended",
                        Step::Slice => "slice",
                        Step::Parked(_) => "parked",
                    },
                    machine.meters(),
                    testing::owner_balance(machine).expect("the source owner balance is readable"),
                ));
                if rebuild && !matches!(step, Step::Ended(_)) {
                    cold.push(
                        testing::semantic_checkpoint(machine)
                            .expect("the cold semantic state is readable"),
                    );
                }
            },
        );
        let exits = testing::take_family_witnesses();
        if mode == Mode::Source {
            assert_eq!(packets.0, 0, "the Python source oracle dispatched packets");
        }
        if mode == Mode::GenericPackets {
            assert_eq!(packets.1, 0, "the generic Python path selected a family");
            assert!(exits.is_empty());
        }
        ((outcome, stops, cold), exits, packets)
    };
    let mut charges = Vec::new();
    for (family, source, program) in &cases {
        let (reference, _, _) = observe(program, BOUNDS, false, Mode::Source);
        assert!(matches!(reference.0.0.end, End::Finished(_)));
        let charged = reference.0.0.charged;
        let (fused, exits, packets) = observe(program, BOUNDS, false, Mode::Optimized);
        assert_eq!(fused, reference, "quiet Python {family:?}: {source}");
        assert!(
            exits.get(&(*family, Exit::Commit)).copied().unwrap_or(0) > 0,
            "the real Python path must commit {family:?}: {exits:?}\n{}",
            kernel_text(source)
        );
        assert!(packets.0 > 0);
        println!(
            "real Python quiet {family:?}: charged={charged}, packets={packets:?}, exits={exits:?}"
        );
        let (generic, _, packets) = observe(program, BOUNDS, false, Mode::GenericPackets);
        assert_eq!(generic, reference, "generic Python {family:?}");
        assert!(packets.0 > 0);
        charges.push(charged);
    }
    for (family, _, program) in &cases {
        for rebuild in [false, true] {
            let (reference, _, _) = observe(program, BOUNDS, rebuild, Mode::Source);
            for mode in [Mode::GenericPackets, Mode::Optimized] {
                let actual = observe(program, BOUNDS, rebuild, mode).0;
                assert_eq!(actual.2.len(), reference.2.len(), "cold boundary count");
                for (at, (actual, expected)) in actual.2.iter().zip(&reference.2).enumerate() {
                    assert!(
                        actual == expected,
                        "Python {family:?}, {mode:?}, cold state at {at}"
                    );
                }
                assert_eq!(
                    actual.1.iter().map(|stop| stop.2).collect::<Vec<_>>(),
                    reference.1.iter().map(|stop| stop.2).collect::<Vec<_>>(),
                    "Python {family:?}, {mode:?}, logical owners at every stop"
                );
                println!(
                    "Python {family:?}, {mode:?}, cold={rebuild}: logical owners and normalized cold states agree"
                );
                assert_eq!(
                    actual.0, reference.0,
                    "Python {family:?}, {mode:?}, terminal observation"
                );
                assert_eq!(
                    actual.1, reference.1,
                    "Python {family:?}, {mode:?}, stop meters"
                );
                assert_eq!(
                    actual, reference,
                    "Python {family:?}, {mode:?}, cold={rebuild}"
                );
            }
        }
    }
    for ((family, _, program), charged) in cases.iter().zip(charges) {
        for charge_limit in 0..charged {
            if charge_limit.is_multiple_of(256) {
                println!("Python {family:?} charge bound {charge_limit}/{charged}");
            }
            let bounds = Bounds {
                charge_limit,
                ..BOUNDS
            };
            let (reference, _, _) = observe(program, bounds, false, Mode::Source);
            for mode in [Mode::GenericPackets, Mode::Optimized] {
                assert_eq!(
                    observe(program, bounds, false, mode).0,
                    reference,
                    "Python {family:?}, {mode:?}, charge={charge_limit}"
                );
            }
        }
        println!("Python {family:?}: all {charged} finite charge limits agree");
    }
}

/// FIG-7430 / K-BND-002: ordinary recursive source must use the standard depth.
#[test]
fn standard_depth_admits_five_thousand_guest_calls() {
    assert_eq!(
        output(
            "def descend(n):\n    if n == 0:\n        return 0\n    return 1 + descend(n - 1)\nprint(descend(5000))\n"
        ),
        (vec!["5000".into()], "ok".into())
    );
}

/// A closed numeric recurrence may add unknown items without selecting
/// list extension. Item kinds remain runtime Python semantics.
#[test]
fn numeric_accumulators_do_not_dispatch_list_extension() {
    let source = "def accumulate():\n    values = [1, 0.5]\n    total = 0\n    for i in range(2):\n        total += values[i]\n    return total\nprint(accumulate())\n";
    assert_eq!(output(source), (vec!["1.5".into()], "ok".into()));
    assert!(
        !calls_helper(source, "py.iadd"),
        "a closed numeric accumulator cannot extend a list"
    );
}
