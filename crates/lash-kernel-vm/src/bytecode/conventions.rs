//! Entry adaptation (`K-DOC-003`, `K-EFF-006`, `K-TASK-019`).
//!
//! TypeScript settlement and a run-root cleanup are epilogues of the entry's own unit, so it adds neither
//! a frame around the body nor a synthetic statement or charge. Ordinary
//! calls of the same function still use its unadapted body.

use lash_kernel_doc::{Datum, Decoder, Document, FunctionTarget, Literal, StartConvention};
use lash_kernel_value::Type;
use lash_kernel_value::sites::Unit as SiteUnit;

use super::compile::{Compiler, Entry, Fun, Result, unsupported};
use super::op::Op;
use super::program::{Block, CallSite, Event, RootCleanup, Unit, UnitId, UnitKind};
use crate::StartError;

/// The unit a start runs: the entry's body under its start convention.
/// Several exports may name one body with different conventions
/// (`K-DOC-003`); the start's own is the one compiled.
pub(crate) fn entry(compiler: &mut Compiler<'_>, entry: &Entry<'_>) -> Result<UnitId> {
    let id = entry.function;
    let await_fn = match entry.convention {
        StartConvention::TsDirect { await_fn } | StartConvention::TsArgsList { await_fn, .. } => {
            await_fn
        }
        StartConvention::PyDirect { cleanup } => return clean_up(compiler, entry, cleanup),
        StartConvention::Direct | StartConvention::PyArgsList { .. } => {
            return match entry.captures {
                None => compiler.body(id, false),
                Some(_) => root(compiler, entry),
            };
        }
    };
    let FunctionTarget::Local(await_fn) = await_fn else {
        return unsupported("a TypeScript entry settlement through an imported function");
    };
    let await_fn = *await_fn;
    let at = root(compiler, entry)?;
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
        checked: None,
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

/// The entry's own unit, apart from the body its calls run: a callable's
/// reads its frozen captures where a closure reads its own (`K-CLO-004`).
fn root(compiler: &mut Compiler<'_>, entry: &Entry<'_>) -> Result<UnitId> {
    let id = entry.function;
    let at = compiler.reserve();
    let kind = match entry.captures {
        Some(_) => UnitKind::Closure(id),
        None => UnitKind::Declared(id),
    };
    compiler.function(id, None, entry.captures, at, false, kind)?;
    Ok(at)
}

/// An entry whose convention declares a run-root cleanup (`K-TASK-019`):
/// once the body has left the root frame by its return, by falling off
/// its end or by its raise, its `finally` blocks run, the cleanup is
/// called with no arguments and its result dropped, and the body's
/// departure goes on, unless the cleanup raised. The body's handlers are
/// gone by then, so a raise of the cleanup leaves the run. The frame holds
/// only the body's return or raise while the cleanup runs.
fn clean_up(
    compiler: &mut Compiler<'_>,
    entry: &Entry<'_>,
    cleanup: &FunctionTarget,
) -> Result<UnitId> {
    let id = entry.function;
    let FunctionTarget::Local(cleanup) = cleanup else {
        return unsupported("a run-root cleanup through an imported function");
    };
    let cleanup = *cleanup;
    let at = root(compiler, entry)?;
    let site = Fun::new(SiteUnit::Library(id), false, false, None);
    let callee = compiler.callee(&site, cleanup)?;
    let mark = compiler.mark(&site, &site.statement);
    let Some(mut unit) = compiler.units[at as usize].take() else {
        return unsupported("an entry with a run-root cleanup has no compiled body");
    };
    let body = unit.ops.len();
    let handlers = unit.ops[..body]
        .iter()
        .filter(|op| matches!(op, Op::TryEnter(_)))
        .count();
    let call = unit.calls.len() as u32;
    unit.calls.push(CallSite {
        callee,
        argc: 0,
        site: site.statement.clone(),
        mark,
        checked: None,
    });
    let mut paths = [0u32; 3];
    for (path, departure) in paths.iter_mut().zip([Op::Return, Op::Fall, Op::Raise]) {
        *path = unit.ops.len() as u32;
        // A return or a fall leaves the body's handlers behind; a raise
        // reaches the cleanup only once none is left.
        let mut call_ops = vec![Op::Block(0)];
        if departure != Op::Raise {
            call_ops.extend(std::iter::repeat_n(Op::TryExit, handlers));
        }
        call_ops.push(Op::Call(call));
        append(&mut unit, &call_ops);
        append(&mut unit, &[Op::Block(0), Op::Pop, departure]);
    }
    let [returned, fell, raised] = paths;
    for op in &mut unit.ops[..body] {
        match op {
            Op::Return => *op = Op::Jump(returned),
            Op::Fall => *op = Op::Jump(fell),
            _ => {}
        }
    }
    unit.cleanup = Some(RootCleanup {
        body: body as u32,
        raised,
    });
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
    unit.step.extend(std::iter::repeat_n(false, ops.len()));
    unit.through.extend(std::iter::repeat_n(0, ops.len()));
    unit.blocks.push(Block {
        start,
        end: unit.ops.len() as u32,
        charge: Some(0),
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

/// A TS entry's returned value spells each `undefined` as JSON does, after
/// settlement.
pub(crate) fn result(convention: Option<&StartConvention>, mut result: Datum) -> Datum {
    if convention.is_some_and(typescript) {
        undefined_as_json(&mut result);
    }
    result
}

/// `value` with each `undefined` spelled as `JSON.stringify` spells it: a
/// record or map member that holds it is left out, and anywhere else,
/// the value itself or a list or tuple element, it is `null`.
pub(crate) fn undefined_as_json(value: &mut Datum) {
    match value {
        Datum::Absent => *value = Datum::Null,
        Datum::List(items) | Datum::Tuple(items) => items.iter_mut().for_each(undefined_as_json),
        Datum::Record(fields) => {
            fields.retain(|(_, field)| !matches!(field, Datum::Absent));
            for (_, field) in fields {
                undefined_as_json(field);
            }
        }
        Datum::Map(entries) => {
            entries.retain(|(_, entry)| !matches!(entry, Datum::Absent));
            for (_, entry) in entries {
                undefined_as_json(entry);
            }
        }
        _ => {}
    }
}
