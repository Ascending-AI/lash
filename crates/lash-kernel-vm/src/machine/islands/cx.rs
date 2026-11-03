//! Canonical operand, text-storage and charge facts used by source publications.
use crate::compile::{Plan, Source};
use crate::data::immediate_measure;
use lash_kernel_doc::arena::{ArenaHandle, ShortText};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::plan::Table;
use lash_kernel_doc::{Args, Measure, Value};
use lash_kernel_state::arena::Arena;
use lash_kernel_value::data_limits::KernelDataLimits;
mod operands;
mod texts;
pub(crate) use operands::{Fresh, Role, position, read_index, scalar};
pub(crate) use texts::prepare_text_prefix;

/// A short text a record's version holds no entry under:
/// an edit that adds an entry revokes the version.
#[derive(Clone, Copy)]
pub(crate) struct Absent {
    table: ArenaHandle,
    key: ShortText,
    version: (u64, u64),
}

/// [`entry`], a record's short key answered absent by the proof in
/// `absent` of the last test that found it absent from this version of the
/// record, and that proof kept where the record holds no entry under it.
/// The proof names the record's identity token and revision, so it stays
/// sound across operations and collections.
#[inline(always)]
pub(crate) fn probe(
    arena: &Arena,
    limits: KernelDataLimits,
    absent: &mut Option<Absent>,
    table: Table,
    target: Value,
    key: Value,
) -> Option<Option<Value>> {
    match (table, target, key) {
        (Table::Record, Value::Record(record), Value::Text(text)) => {
            probed(arena, limits, absent, record, text)
        }
        _ => entry(arena, limits, table, target, key),
    }
}

/// [`probe`] of record `record`'s short key `text`, out of line so that
/// the other tables' lookups stay [`entry`]'s.
#[inline(never)]
fn probed(
    arena: &Arena,
    limits: KernelDataLimits,
    absent: &mut Option<Absent>,
    record: ArenaHandle,
    text: ShortText,
) -> Option<Option<Value>> {
    if let Some(proof) = *absent
        && proof.table == record
        && proof.key == text
        && !arena.any_cold()
        && arena.table_is(record, proof.version)
    {
        return Some(None);
    }
    let found = entry(
        arena,
        limits,
        Table::Record,
        Value::Record(record),
        Value::Text(text),
    )?;
    if found.is_none()
        && let Ok(Some(version)) = arena.table_version(record)
    {
        *absent = Some(Absent {
            table: record,
            key: text,
            version,
        });
    }
    Some(found)
}

/// `target`'s entry under `key`, looked up as `<table>.contains`'s call
/// looks it up, through the same views, without its call: it builds and
/// reserves nothing. `Some(None)` where the table holds no `key`, and a
/// set's member is `true`, as `target[key]` reads them; `None` where the
/// call raises or must decode first.
fn entry(
    arena: &Arena,
    limits: KernelDataLimits,
    table: Table,
    target: Value,
    key: Value,
) -> Option<Option<Value>> {
    if !matches!(
        (table, target),
        (Table::Map, Value::Map(_))
            | (Table::Set, Value::Set(_))
            | (Table::Record, Value::Record(_))
    ) {
        return None;
    }
    // A short text is a legal key of every table, and its own key.
    if let (Value::Text(text), Some(handle)) = (key, target.handle()) {
        let found = arena.table_get_text(handle, &text).ok()?;
        return Some(found.map(|value| match table {
            Table::Set => Value::Bool(true),
            Table::Map | Table::Record => value,
        }));
    }
    let semantics = arena.semantics().with_limits(limits);
    let args = Args::new(&[], arena.objects(), &semantics);
    let view = args.table(&target).ok()??;
    match table {
        Table::Record if !matches!(key, Value::Text(_) | Value::LongText(_)) => return None,
        Table::Record => {}
        Table::Map | Table::Set => legal_key(&args, &key)?,
    }
    let found = args.lookup(view, &key).ok()?;
    Some(found.map(|value| match table {
        Table::Set => Value::Bool(true),
        Table::Map | Table::Record => *value,
    }))
}

/// What `plan` charges a call of `args` that gave `result`, measured in
/// `arena` as the reference measures it; `None` when a value cannot be
/// measured where it is.
pub(crate) fn measured_formula(
    arena: &Arena,
    plan: &Plan,
    args: &[Value],
    result: Option<&Value>,
) -> Option<u64> {
    measured_formula_by(plan, |source, measure| {
        let value = match source {
            Source::Arg(index) => args.get(index),
            Source::Result => result,
            Source::Nothing => None,
        };
        value.map_or(Some(0), |value| value_measure(arena, value, measure))
    })
}

fn measured_formula_by(
    plan: &Plan,
    mut measure: impl FnMut(Source, Measure) -> Option<u64>,
) -> Option<u64> {
    let mut failed = false;
    let units = plan.evaluate(|source, kind| {
        if failed {
            return 0;
        }
        measure(source, kind).unwrap_or_else(|| {
            failed = true;
            0
        })
    });
    (!failed).then_some(units)
}

fn value_measure(
    arena: &impl crate::data::ChargeArena,
    value: &Value,
    measure: Measure,
) -> Option<u64> {
    if let Some(units) = immediate_measure(value, measure) {
        return Some(units);
    }
    match measure {
        Measure::Size => crate::data::view_size(arena, value),
        Measure::DeepSize => crate::data::view_deep_size(arena, value),
        Measure::NestedSize => crate::data::view_nested_size(arena, value),
        Measure::Magnitude => Ok(crate::data::view_magnitude(arena, value)),
    }
    .ok()
}

/// `key` where it is a legal key as `map.contains` and `set.contains`
/// check one: `None` where their call raises (`invalid_key`, `too_deep`).
fn legal_key(args: &Args<'_>, key: &Value) -> Option<()> {
    // An inline scalar or short text is a legal key by its kind.
    if matches!(
        key,
        Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Timestamp(_)
            | Value::Function(_)
            | Value::TaskRef(_)
    ) {
        return Some(());
    }
    if matches!(key, Value::Tuple(_))
        && args.depth(key) as usize > args.limits().value_depth().min(128)
    {
        return None;
    }
    args.normal_key(key).ok()?.map(|_| ())
}
