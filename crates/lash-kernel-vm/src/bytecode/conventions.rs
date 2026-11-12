//! Entry adaptation (`K-DOC-003`, `K-EFF-006`, `K-TASK-019`).
//!
//! TypeScript settlement and a run-root cleanup are epilogues of the entry's own unit, so it adds neither
//! a frame around the body nor a synthetic statement or charge. Ordinary
//! calls of the same function still use its unadapted body.

use std::sync::Arc;

use lash_kernel_doc::{Datum, Decoder, Document, FunctionTarget, Literal, StartConvention};
use lash_kernel_value::Type;
use lash_kernel_value::names::{CallableId, FunctionId, ImportSlot};
use lash_kernel_value::sites::Unit as SiteUnit;

use super::compile::{Compiler, Entry, Fun, Result, unsupported};
use super::op::Op;
use super::program::{
    Adapt, Block, CallSite, Callee, Checked, Event, RootCleanup, Unit, UnitId, UnitKind,
};
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
                None if called(compiler, id) => root(compiler, entry),
                None => compiler.entry_body(id),
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
    let Some(mut unit) = compiler.take(at) else {
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
        entry: None,
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
    compiler.put(at, unit);
    Ok(at)
}

/// The entry's own unit, apart from the body its calls run: a callable's
/// reads its frozen captures where a closure reads its own (`K-CLO-004`).
///
/// A function a call bills by its formula starts through a call of it,
/// as any caller's call runs it: the unit passes the start's arguments
/// on, the call runs what a call of it runs and pays the function's
/// formula, and the start copies its result out (`K-HOST`, `K-CHG-002`,
/// `K-CHG-007`). No Main is needed, nor a guest body for a native.
fn root(compiler: &mut Compiler<'_>, entry: &Entry<'_>) -> Result<UnitId> {
    let id = entry.function;
    if entry.captures.is_none() && called(compiler, id) {
        let params = compiler.def(&id)?.signature.params.len();
        let Ok(argc) = u16::try_from(params) else {
            return unsupported("a native entry with more parameters than a call passes");
        };
        let site = Fun::new(SiteUnit::Library(id), false, false, None);
        if compiler.def(&id)?.body().is_some() {
            return billed(compiler, id, site, argc);
        }
        let callable = lash_kernel_value::names::CallableId {
            link: compiler.link,
            function: id,
        };
        return compiler.stub(&site, Some(callable), None, argc);
    }
    let at = compiler.reserve();
    let kind = match entry.captures {
        Some(_) => UnitKind::Closure(id),
        None => UnitKind::Declared(id),
    };
    compiler.function(id, None, entry.captures, at, false, kind)?;
    Ok(at)
}

/// The start of a formula-billed `id` with a guest body: a unit that
/// passes the start's arguments to a call of that body, which runs its
/// statements covered and pays the formula. A function that has a
/// native as well still runs its body, as a call of it does on the
/// reference engine.
fn billed(compiler: &mut Compiler<'_>, id: FunctionId, mut g: Fun, argc: u16) -> Result<UnitId> {
    let definition = compiler.def(&id)?;
    let at = compiler.reserve();
    g.locals = argc;
    g.leader();
    for value in 0..argc {
        g.emit(Op::Local(value));
    }
    let callee = Callee::Unit {
        unit: compiler.body(id, true)?,
        charge: compiler.charge(id, definition),
    };
    let site = g.site();
    compiler.call(&mut g, callee, argc, site);
    g.emit(Op::Return);
    compiler.seal(g, at, UnitKind::Spawn, argc, Arc::from("spawn"));
    Ok(at)
}

/// Whether a start of `id` runs as a call of it: a native is registered
/// for it, it has no kernel body, or a call bills its formula.
fn called(compiler: &Compiler<'_>, id: FunctionId) -> bool {
    compiler
        .registry
        .get(&id)
        .is_some_and(|registered| registered.native().is_some())
        || compiler
            .def(&id)
            .is_ok_and(|definition| definition.body().is_none() || definition.charge().is_some())
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
    let Some(mut unit) = compiler.take(at) else {
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
        entry: None,
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
    compiler.put(at, unit);
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

/// Decodes TS and packed Python source parameters before packing the
/// private calling convention. Other conventions retain the machine's
/// existing decoding path.
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
    let packed = matches!(export.convention, StartConvention::PyArgsList { .. });
    if !typescript(&export.convention) && !packed {
        return Ok(None);
    }
    if packed && raw.len() > export.signature.params.len() {
        return Err(StartError::Arguments {
            problem: format!(
                "expected at most {} arguments, got {}",
                export.signature.params.len(),
                raw.len()
            ),
        });
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
                    .map_err(refused(code.link, export, param))?,
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
        // The body takes its positional arguments as a list, the rest by
        // name, and the active exception (none at a start).
        StartConvention::PyArgsList { positional } => {
            let keywords = export
                .signature
                .params
                .iter()
                .zip(args.iter().cloned())
                .skip(positional as usize)
                .map(|(param, value)| (Datum::Text(param.name.as_str().to_owned()), value))
                .collect();
            args.truncate(positional as usize);
            Ok(Some(vec![
                Datum::List(args),
                Datum::Map(keywords),
                Datum::Absent,
            ]))
        }
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

/// A start argument's refusal: one that does not fit its parameter's type
/// is the entry's typed mismatch at `input/<parameter>` (FIG-7143); any
/// other refusal is the decoder's.
pub(crate) fn refused(
    link: lash_kernel_value::content_ids::LinkId,
    export: &lash_kernel_doc::FunctionExport,
    param: &lash_kernel_value::Param,
) -> impl FnOnce(lash_kernel_doc::DecodeRefusal) -> StartError {
    move |refusal| match refusal {
        lash_kernel_doc::DecodeRefusal::Type { mismatch } => {
            StartError::TypeMismatch(Box::new(lash_kernel_value::EntryTypeMismatch {
                callable: lash_kernel_value::CallableId {
                    link,
                    function: export.function,
                },
                mismatch: mismatch.within(param.name.as_str()).within("input"),
            }))
        }
        other => StartError::Decode(other),
    }
}

pub(crate) fn typescript(convention: &StartConvention) -> bool {
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

/// An export's entry unit, how a call through an import adapts its
/// arguments to it (none for a `PyDirect` export), and the contract the
/// call holds to.
type Adapted = (UnitId, Option<Adapt>, Arc<Checked>);

impl Compiler<'_> {
    /// What a call from `f` through import `slot` calls, and the contract
    /// it holds to. An export is entered as a start enters it, under its
    /// start convention (`K-DOC-003`): one that adapts its arguments,
    /// settles its result or cleans up after its body runs its own entry
    /// unit, in its own Link, its arguments checked against its public
    /// signature and then packed at the call. A `Direct` export's call is
    /// a call of its function, held to the import's signature (FIG-7143).
    pub(crate) fn import_call(
        &mut self,
        f: &Fun,
        slot: ImportSlot,
    ) -> Result<(Callee, Option<Arc<Checked>>)> {
        let Some((callable, export)) = self.imports.get(&(self.link, slot)).cloned() else {
            return unsupported(format!("import {slot:?} names no function"));
        };
        let home = self.link;
        self.enter_link(callable.link)?;
        let adapted = self.adapted(callable, &export);
        self.enter_link(home)?;
        match adapted? {
            Some((unit, adapt, checked)) => Ok((
                match adapt {
                    Some(adapt) => Callee::Entry { unit, adapt },
                    None => Callee::Unit { unit, charge: None },
                },
                Some(checked),
            )),
            None => Ok((
                self.callee_at(f, callable)?,
                self.import_checked(slot, callable),
            )),
        }
    }

    /// The entry unit of export `export` of the Link being compiled, which
    /// names `callable`, how a call adapts its arguments to it, and the
    /// contract a call of it holds to; none for a `Direct` export.
    fn adapted(
        &mut self,
        callable: CallableId,
        export: &lash_kernel_value::names::ExportName,
    ) -> Result<Option<Adapted>> {
        use lash_kernel_doc::StartConvention;
        let Some(descriptor) = crate::interface::exported(self.document, export) else {
            return unsupported("an import names an absent export");
        };
        let function = callable.function;
        let adapt = match descriptor.convention {
            StartConvention::Direct => return Ok(None),
            StartConvention::PyDirect { .. } => None,
            StartConvention::TsDirect { .. } => {
                let arity = self.def(&function)?.signature.params.len();
                let Ok(arity) = u16::try_from(arity) else {
                    return unsupported("an export with more parameters than a call passes");
                };
                Some(Adapt::Slots(arity))
            }
            StartConvention::TsArgsList { .. } => Some(Adapt::Listed),
            StartConvention::PyArgsList { positional } => Some(Adapt::Python {
                positional: u16::try_from(positional).unwrap_or(u16::MAX),
            }),
        };
        let key = (function, descriptor.convention.clone());
        let unit = match self.scope.entries.get(&key) {
            Some(unit) => *unit,
            None => {
                let unit = super::conventions::entry(
                    self,
                    &Entry {
                        function,
                        convention: &descriptor.convention,
                        captures: None,
                    },
                )?;
                self.entered.insert(unit);
                self.scope.entries.insert(key, unit);
                unit
            }
        };
        let checked = Checked::new(callable, descriptor.signature.clone());
        Ok(Some((unit, adapt, Arc::new(checked))))
    }
}
