//! TypeScript entry adaptation (`K-DOC-003`, `K-EFF-006`).
//!
//! Settlement is an epilogue of the entry's own unit, so it adds neither
//! a frame around the body nor a synthetic statement or charge. Ordinary
//! calls of the same function still use its unadapted body.

use lash_kernel_doc::{Datum, Decoder, Document, Export, FunctionTarget, Literal, StartConvention};
use lash_kernel_value::sites::Unit as SiteUnit;
use lash_kernel_value::{FunctionId, Type};

use super::compile::{Compiler, Fun, Result, unsupported};
use super::op::Op;
use super::program::{Block, CallSite, Event, Unit, UnitId, UnitKind};
use crate::StartError;

/// Compiles the convention of the selected export. Several exports may name
/// one body with different start signatures and conventions (`K-DOC-003`).
/// Only the export lookup is narrowed; code identities still use the original
/// Link, and every body, import and constant is unchanged.
pub(crate) fn compile_selected(
    document: &Document,
    registry: &lash_kernel_doc::FunctionRegistry,
    link: lash_kernel_value::LinkId,
    others: &[(lash_kernel_value::LinkId, &Document)],
    entry: Option<FunctionId>,
    convention: Option<&StartConvention>,
    available: Option<&lash_kernel_doc::BuildHelpers>,
) -> Result<super::program::Program> {
    let mut selected = std::borrow::Cow::Borrowed(document);
    if let (Some(entry), Some(convention)) = (entry, convention)
        && document.exports.values().any(|export| {
            matches!(export, Export::Function(export) if export.function == entry && &export.convention != convention)
        })
    {
        selected.to_mut().exports.retain(|_, export| {
            !matches!(export, Export::Function(export) if export.function == entry && &export.convention != convention)
        });
    }
    super::compile::compile(&selected, registry, link, others, entry, available)
}

/// Selects an entry's unit, retaining the existing path for other conventions.
pub(crate) fn entry(compiler: &mut Compiler<'_>, id: FunctionId) -> Result<UnitId> {
    let convention = compiler
        .document
        .exports
        .values()
        .find_map(|export| match export {
            Export::Function(export) if export.function == id => Some(&export.convention),
            _ => None,
        });
    let Some(StartConvention::TsDirect { await_fn } | StartConvention::TsArgsList { await_fn, .. }) =
        convention
    else {
        return compiler.body(id, false);
    };
    let FunctionTarget::Local(await_fn) = await_fn else {
        return unsupported("a TypeScript entry settlement through an imported function");
    };
    let await_fn = *await_fn;
    let at = compiler.reserve();
    compiler.function(id, None, at, false, UnitKind::Declared(id))?;
    let site = Fun::new(SiteUnit::Library(id), false, false, None);
    let callee = compiler.callee(&site, await_fn)?;
    let mark = compiler.mark(&site, &site.statement);
    let null = compiler.constant(&Literal::Null);
    let Some(mut unit) = compiler.units[at as usize].take() else {
        return unsupported("a TypeScript entry has no compiled body");
    };
    let returns = unit.ops.len();
    let fall = unit.ops.len() as u32;
    append(&mut unit, &[Op::Block(0), Op::Const(null)]);
    let settle = unit.ops.len() as u32;
    let call = unit.calls.len() as u32;
    unit.calls.push(CallSite {
        callee,
        argc: 1,
        site: site.statement.clone(),
        mark,
    });
    // Settlement runs after the body's handlers have left, like its return.
    // This is the root unit: there are no caller handlers beneath its own.
    let handlers = unit.ops[..returns]
        .iter()
        .filter(|op| matches!(op, Op::TryEnter(_)))
        .count();
    let mut epilogue = vec![Op::Block(0)];
    epilogue.extend(std::iter::repeat_n(Op::TryExit, handlers));
    epilogue.push(Op::Call(call));
    append(&mut unit, &epilogue);
    append(&mut unit, &[Op::Block(0), Op::Return]);
    for op in &mut unit.ops[..returns] {
        match op {
            Op::Return => *op = Op::Jump(settle),
            Op::Fall => *op = Op::Jump(fall),
            _ => {}
        }
    }
    // The fall block continues into settlement with its implicit null.
    compiler.units[at as usize] = Some(unit);
    Ok(at)
}

/// Appends one zero-event block without changing any existing coordinate.
fn append(unit: &mut Unit, ops: &[Op]) {
    let start = unit.ops.len() as u32;
    unit.ops.extend_from_slice(ops);
    unit.ops[start as usize] = Op::Block(unit.blocks.len() as u32);
    unit.event
        .extend(std::iter::repeat_n(Event::default(), ops.len()));
    unit.pace.extend(std::iter::repeat_n(0, ops.len()));
    unit.step.extend(std::iter::repeat_n(false, ops.len()));
    unit.through.extend(std::iter::repeat_n(0, ops.len()));
    unit.pace_through.extend(std::iter::repeat_n(0, ops.len()));
    unit.blocks.push(Block {
        start,
        end: unit.ops.len() as u32,
        charge: Some(0),
        pace: Some(0),
    });
}

/// Decodes TS source parameters before packing the private calling convention.
/// Other conventions retain the machine's existing decoding path.
pub(crate) fn arguments(
    document: &Document,
    code: &super::program::Program,
    export: Option<&lash_kernel_doc::FunctionExport>,
    raw: &[Datum],
    max_depth: usize,
) -> std::result::Result<Option<Vec<Datum>>, StartError> {
    let Some(export) = export else {
        return Ok(None);
    };
    if !typescript(&export.convention) {
        return Ok(None);
    }
    let decoder = Decoder {
        max_depth,
        policy: document.numbers,
        declared: &|callable| code.callable(callable),
    };
    let mut args = Vec::new();
    for (at, param) in export.signature.params.iter().enumerate() {
        match raw.get(at) {
            Some(Datum::Absent) if param.optional => args.push(Datum::Absent),
            Some(value) => args.push(
                decoder
                    .decode(value, &param.ty)
                    .map_err(StartError::Decode)?,
            ),
            None if param.optional => break,
            None => {
                return Err(StartError::MissingArgument {
                    name: param.name.clone(),
                });
            }
        }
    }
    match export.convention {
        StartConvention::TsArgsList { .. } => {
            for value in raw.iter().skip(export.signature.params.len()) {
                args.push(
                    decoder
                        .decode(value, &Type::Any)
                        .map_err(StartError::Decode)?,
                );
            }
            Ok(Some(vec![Datum::Absent, Datum::List(args)]))
        }
        StartConvention::TsDirect { .. } => {
            let arity = document
                .functions
                .get(&export.function)
                .ok_or_else(|| StartError::Linked {
                    problem: "a TS entry has no declared body".into(),
                })?
                .signature
                .params
                .len();
            args.truncate(arity);
            args.resize(arity, Datum::Absent);
            Ok(Some(args))
        }
        _ => Ok(None),
    }
}

fn typescript(convention: &StartConvention) -> bool {
    matches!(
        convention,
        StartConvention::TsDirect { .. } | StartConvention::TsArgsList { .. }
    )
}

/// Only a TS entry's returned absent is projected to null, after settlement.
pub(crate) fn result(convention: Option<&StartConvention>, result: Datum) -> Datum {
    if convention.is_some_and(typescript) && matches!(result, Datum::Absent) {
        Datum::Null
    } else {
        result
    }
}
