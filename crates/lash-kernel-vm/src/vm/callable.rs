//! A start on a callable (`K-CLO-004`): a function value a run published,
//! its frozen captures held apart from the run that made it. The start
//! runs the callable's body as the root of a new run, under the
//! callable's start convention, with every function value its captures
//! hold made again over fresh cells: a callable start shares no variable
//! with the run that froze it, or with another start of it.

use std::sync::Arc;

use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::arena::{ArenaHandle, ClosureIdentity};
use lash_kernel_doc::linked_site::LinkedSite;
use lash_kernel_doc::{
    CaptureValue, Datum, FunctionExport, FunctionRepresentation, FunctionTarget, FunctionValue,
    StartConvention, Value,
};
use lash_kernel_value::names::{CallableId, Name};
use lash_kernel_value::sites::{Site, Unit as SiteUnit};

use super::datum::{Build, copy_in};
use super::{Run, Vm, fault};
use crate::heap::Heap;
use crate::interface::{Program, StartError};

/// What a start runs and the code its frozen captures need.
pub(crate) struct Root<'a> {
    pub entry: crate::bytecode::compile::Entry<'a>,
    /// The closures a callable's captures hold, its own first when it
    /// captures.
    pub closures: Vec<ClosureIdentity>,
    /// The bare function values they hold.
    pub functions: Vec<CallableId>,
}

/// The export a start on `function` runs: the callable checked against
/// the run's root Link, as the definition that made it presents it.
pub(crate) fn export(
    program: &Program,
    function: &FunctionValue,
    limits: lash_kernel_value::data_limits::KernelDataLimits,
) -> Result<FunctionExport, StartError> {
    let arguments = |problem: &str| StartError::Arguments {
        problem: problem.into(),
    };
    if function.callable.link != program.link {
        return Err(arguments("callable Link differs from the program root"));
    }
    let id = function.callable.function;
    let signature = program
        .document
        .functions
        .get(&id)
        .map(|definition| definition.signature.clone())
        .or_else(|| {
            program
                .helpers()
                .registered(&id)
                .map(|registered| registered.header().signature.clone())
        })
        .ok_or_else(|| arguments("callable definition is absent from the defining Link"))?;
    lash_kernel_doc::validate_callable_signature(
        &function.signature,
        &function.convention,
        &signature,
    )
    .map_err(StartError::NotAdmitted)?;
    let helpers = match &function.convention {
        StartConvention::Direct | StartConvention::PyArgsList { .. } => Vec::new(),
        StartConvention::TsDirect { await_fn } => vec![await_fn],
        StartConvention::TsArgsList { await_fn, same_fn } => vec![await_fn, same_fn],
        StartConvention::PyDirect { cleanup } => vec![cleanup],
    };
    for target in helpers {
        let declared = match target {
            FunctionTarget::Local(id) => {
                program.document.functions.contains_key(id) || program.helpers().contains(id)
            }
            FunctionTarget::Import(slot) => {
                program.document.imports.get(slot).is_some_and(|import| {
                    matches!(import.kind, lash_kernel_value::ImportKind::Code { .. })
                })
            }
        };
        if !declared {
            return Err(arguments(
                "callable convention helper is absent from its defining Link",
            ));
        }
    }
    function
        .check(
            program.library.registry().as_ref(),
            lash_kernel_doc::FunctionValueLimits::standard(),
            limits,
        )
        .map_err(|error| arguments(&error.to_string()))?;
    // Starting an exported callable is not workflow promotion: workflow
    // hosts apply its entry export before the machine sees it.
    let defining = function
        .check_definition(&program.document, program.helpers(), &program.selection)
        .map_err(StartError::FunctionValue)?;
    Ok(FunctionExport {
        function: id,
        signature: defining.signature,
        convention: defining.convention,
        asynchronous: false,
        rest: false,
    })
}

/// The names `function` captures, in its closure's order.
pub(crate) fn names(function: &FunctionValue) -> Vec<Name> {
    function.captures.keys().cloned().collect()
}

/// A function value of the graph that is a closure: one made where it
/// stands, or one over captures.
fn closes(function: &FunctionValue) -> bool {
    function.creation.is_some() || !function.captures.is_empty()
}

/// The closure `function` is made again as.
fn identity(function: &FunctionValue) -> ClosureIdentity {
    ClosureIdentity {
        capture_names: names(function),
        callable: function.callable,
        site: LinkedSite {
            link: function.callable.link,
            site: function.creation.clone().unwrap_or_else(|| {
                Site::new(SiteUnit::Library(function.callable.function), Vec::new())
            }),
        },
    }
}

/// The code the function values `function`'s captures hold need.
pub(crate) fn held(function: &FunctionValue) -> (Vec<ClosureIdentity>, Vec<CallableId>) {
    let mut closures = Vec::new();
    let mut functions = Vec::new();
    for node in function.nodes().into_iter().skip(1) {
        if closes(node) {
            closures.push(identity(node));
        } else {
            functions.push(node.callable);
        }
    }
    (closures, functions)
}

/// One function value of the graph, made again.
struct Node<'a> {
    function: &'a FunctionValue,
    cells: Vec<ArenaHandle>,
    value: Value,
}

/// Makes `function`'s graph again in `vm`: every function value it holds
/// over fresh cells, then every cell filled, so its references may form
/// any cycle. Gives the root's closure.
pub(crate) fn restore<H: Heap>(vm: &mut Vm<H>, function: &FunctionValue) -> Run<ArenaHandle> {
    let mark = vm.held().len();
    let functions = function.nodes();
    let mut nodes: Vec<Node<'_>> = Vec::with_capacity(functions.len());
    for (at, node) in functions.into_iter().enumerate() {
        let (cells, callable) = if at == 0 || closes(node) {
            // One admission for the closure and its cells: nothing
            // collects between a cell and the closure that holds it.
            let count = node.captures.len();
            vm.admit(
                crate::heap::meter::CELL
                    .saturating_mul(count as u64)
                    .saturating_add(crate::heap::meter::closure(count)),
            )?;
            let cells: Vec<ArenaHandle> = (0..count).map(|_| vm.heap.cell(Value::Null)).collect();
            let closure = vm.heap.closure(Arc::new(identity(node)), cells.clone());
            (cells, Value::Closure(closure))
        } else {
            (Vec::new(), vm.alloc(NewObject::Function(node.callable))?)
        };
        vm.held().push(callable);
        let value = match (&node.representation, &node.token) {
            (FunctionRepresentation::Bare, None) => callable,
            (
                FunctionRepresentation::Token {
                    tag,
                    callable: slot,
                },
                Some(token),
            ) => {
                let slot = *slot as usize;
                if token.first() != Some(&Datum::Text(tag.clone()))
                    || slot >= token.len()
                    || token[slot] != Datum::Null
                {
                    return Err(fault("invalid captured callable token"));
                }
                let mut members = Vec::with_capacity(token.len());
                for (index, datum) in token.iter().enumerate() {
                    let member = if index == slot {
                        callable
                    } else {
                        copy_in(vm, datum)?
                    };
                    vm.held().push(member);
                    members.push(member);
                }
                vm.alloc(NewObject::Tuple(members))?
            }
            _ => return Err(fault("callable token differs from its representation")),
        };
        vm.held().push(value);
        nodes.push(Node {
            function: node,
            cells,
            value,
        });
    }
    for at in 0..nodes.len() {
        let function = nodes[at].function;
        for (index, capture) in function.captures.values().enumerate() {
            let value = capture_value(vm, capture, &nodes)?;
            let cell = nodes[at].cells[index];
            vm.heap
                .cell_set(cell, value)
                .map_err(|error| vm.native(error))?;
        }
    }
    let root = match nodes.first().map(|node| node.value) {
        Some(Value::Closure(root)) => root,
        _ => return Err(fault("a callable's graph has no root closure")),
    };
    vm.held().truncate(mark);
    Ok(root)
}

/// A frozen capture made again, held until its cell holds it.
fn capture_value<H: Heap>(
    vm: &mut Vm<H>,
    capture: &CaptureValue,
    nodes: &[Node<'_>],
) -> Run<Value> {
    let value = match capture {
        CaptureValue::Data(datum) => copy_in(vm, datum)?,
        CaptureValue::Reference(index) => {
            nodes
                .get(*index as usize)
                .ok_or_else(|| fault("callable reference is outside the graph"))?
                .value
        }
        // The node this value was encoded as, by its place in the graph:
        // equal code may close over different captures.
        CaptureValue::Function(function) => {
            nodes
                .iter()
                .find(|node| std::ptr::eq(node.function, function.as_ref()))
                .ok_or_else(|| fault("callable node was not made"))?
                .value
        }
        CaptureValue::Tuple(members) | CaptureValue::List(members) | CaptureValue::Set(members) => {
            let members = held_all(vm, members.iter(), nodes)?;
            vm.alloc(match capture {
                CaptureValue::Tuple(_) => NewObject::Tuple(members),
                CaptureValue::List(_) => NewObject::List(members),
                _ => NewObject::Set(members),
            })?
        }
        CaptureValue::Map(pairs) => {
            let flat = held_all(vm, pairs.iter().flat_map(|(k, v)| [k, v]), nodes)?;
            let pairs = flat.chunks(2).map(|pair| (pair[0], pair[1])).collect();
            vm.alloc(NewObject::Map(pairs))?
        }
        CaptureValue::Record(fields) => {
            let mut pairs = Vec::with_capacity(fields.len());
            for (name, member) in fields {
                let key = vm.text(name)?;
                vm.held().push(key);
                let member = capture_value(vm, member, nodes)?;
                pairs.push((key, member));
            }
            vm.alloc(NewObject::Record(pairs))?
        }
        CaptureValue::Error {
            kind,
            message,
            data,
        } => {
            let data = capture_value(vm, data, nodes)?;
            vm.new_error(kind, message, data)?
        }
    };
    vm.held().push(value);
    Ok(value)
}

fn held_all<'c, H: Heap>(
    vm: &mut Vm<H>,
    members: impl Iterator<Item = &'c CaptureValue>,
    nodes: &[Node<'_>],
) -> Run<Vec<Value>> {
    members
        .map(|member| capture_value(vm, member, nodes))
        .collect()
}
