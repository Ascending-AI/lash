//! A session cell's end (SAVE-IN-DATA, SAVE-KEY, K-SES-002): every closure
//! a carried binding reaches, as its value, inside data, or inside data a
//! capture reaches, is saved once, keyed by its object, and every slot that
//! held it holds a reference to its saved function, `Function(name)`. A
//! closure only a capture holds, as its whole value, is a helper made again
//! in each function that reads it, as lowering makes one.
//!
//! A closure a binding holds as its own value is saved under the first
//! binding name that holds it; any other is saved under a generated name,
//! [`Name::saved`], which no source can declare. The slots are replaced
//! where they are, so every object the bindings share stays shared, and
//! two bindings that held one closure hold one saved function.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use lash_kernel_doc::Action;
use lash_kernel_doc::Callee;
use lash_kernel_doc::Expr;
use lash_kernel_doc::Rhs;
use lash_kernel_doc::Stmt;
use lash_kernel_doc::heap::{Element, EntityId, HeapFault, Identity, PageReader, PageValue};
use lash_kernel_doc::snapshot::{CodeReads, SnapshotHeap as _, closure_at};
use lash_kernel_doc::snapshot::{closure_held, freeze_token, members, reach, token_closure};
use lash_kernel_state::kernel_internals::{CARD, PagedHeap, StoredCards};
use lash_kernel_state::paged::{FieldName, HeapWriteError, ObjectKind, SemanticHeap};
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::sites::Unit;

use super::{
    HeldAt, Left, NotSaved, SaveError, SavedFunction, Saving, Token, function_reference,
    save_closure,
};

/// What a cell's end saves.
#[derive(Debug, Default)]
pub struct Saved {
    /// Each saved function once, by the name it is declared under.
    pub functions: BTreeMap<Name, SavedFunction>,
    /// The value each binding the carry dropped for a closure is bound to
    /// instead: its own, each closure in it a reference to its saved
    /// function.
    pub bindings: BTreeMap<Name, PageValue>,
    /// Why each other binding the carry dropped is not carried.
    pub refused: BTreeMap<Name, NotSaved>,
}

/// Saves every closure the bindings `left.not_carried` reach, each once,
/// and binds each of those bindings to its value with every closure in it
/// a reference to its saved function. `held` names the functions the
/// session already holds: what a saved function may still use, and the
/// generated names already taken.
///
/// # Errors
///
/// The heap's faults.
pub fn save(
    heap: &mut PagedHeap,
    left: Left<'_>,
    held: &BTreeSet<Name>,
) -> Result<Saved, HeapFault> {
    let tag = left.values.tag();
    let mut named: BTreeMap<EntityId, Name> = BTreeMap::new();
    for (name, value) in left.bindings {
        if let Some(id) = closure_held(heap, value, tag)? {
            named.entry(id).or_insert_with(|| name.clone());
        }
    }
    let roots: Vec<&Name> = left
        .not_carried
        .iter()
        .filter(|name| left.bindings.contains_key(*name))
        .collect();
    let found = reach(
        &mut Saving {
            heap: &mut *heap,
            left: &left,
        },
        left.document,
        tag,
        roots
            .iter()
            .map(|name| (name.to_string(), left.bindings[*name].clone()))
            .collect(),
        &named,
    )?;
    let first = held
        .iter()
        .filter_map(Name::saved_index)
        .max()
        .map_or(0, |n| n + 1);
    let mut places = BTreeMap::new();
    for (next, found) in (first..).zip(found) {
        named.insert(found.closure, Name::saved(next));
        places.insert(
            found.closure,
            HeldAt {
                binding: roots[found.root].clone(),
                path: found.path,
            },
        );
    }

    let owners = helper_owners(heap, &left, &named)?;
    let mut functions: BTreeMap<EntityId, SavedFunction> = BTreeMap::new();
    let mut refusals: BTreeMap<EntityId, NotSaved> = BTreeMap::new();
    for (&id, name) in &named {
        match save_one(heap, &left, name, id, &named, &owners) {
            Ok(mut function) => {
                function.held = places.get(&id).cloned();
                functions.insert(id, function);
            }
            Err(SaveError::Refused(why)) => {
                refusals.insert(id, why);
            }
            Err(SaveError::Heap(fault)) => return Err(fault),
        }
    }
    // A function that uses one neither saved here nor still held is not
    // saved, to a fixed point.
    let ids: BTreeMap<&Name, EntityId> = named.iter().map(|(id, name)| (name, *id)).collect();
    loop {
        let mut missing = None;
        'functions: for (id, function) in &functions {
            for needed in function.needs() {
                let saved = ids
                    .get(&needed)
                    .is_some_and(|id| functions.contains_key(id));
                if !saved && !still_held(heap, &left, held, &needed)? {
                    missing = Some((*id, needed));
                    break 'functions;
                }
            }
        }
        let Some((id, needed)) = missing else {
            break;
        };
        functions.remove(&id);
        let function = ids
            .get(&needed)
            .and_then(|id| places.get(id))
            .map_or(needed.clone(), HeldAt::label);
        refusals.insert(id, NotSaved::Needs { function });
    }

    let mut saved = Saved::default();
    let mut carried = Vec::new();
    let mut reached = BTreeSet::new();
    for name in left.not_carried {
        let Some(value) = left.bindings.get(name) else {
            saved.refused.insert(name.clone(), NotSaved::Task);
            continue;
        };
        match holds(heap, value, &named)? {
            Holds::Task => {
                saved.refused.insert(name.clone(), NotSaved::Task);
            }
            Holds::Keyed => {
                saved.refused.insert(name.clone(), NotSaved::Keyed);
            }
            Holds::Closures(closures) => {
                if let Some(why) = closures.iter().find_map(|id| refusals.get(id)) {
                    saved.refused.insert(name.clone(), why.clone());
                } else {
                    reached.extend(closures);
                    carried.push(name);
                }
            }
        }
    }
    let mut replaced = BTreeSet::new();
    for name in carried {
        let value = &left.bindings[name];
        let referred = refer(heap, value, &named, &mut replaced)
            .map_err(write_fault)?
            .unwrap_or_else(|| value.clone());
        saved.bindings.insert(name.clone(), referred);
    }
    // The functions the carried bindings reach and every one they use.
    let mut pending: Vec<EntityId> = reached.into_iter().collect();
    while let Some(id) = pending.pop() {
        let Some(function) = functions.remove(&id) else {
            continue;
        };
        pending.extend(function.needs().iter().filter_map(|n| ids.get(n).copied()));
        saved.functions.insert(function.name.clone(), function);
    }
    Ok(saved)
}

/// The named functions that forward to each helper (FIG-6137): a named
/// closure whose body only calls the closure one capture holds and returns
/// what it answers owns that closure, so a function that makes the same
/// helper in its own body uses the owner, and mutually recursive functions
/// whose calls go through such private bodies stay one bundle.
fn helper_owners(
    heap: &mut PagedHeap,
    left: &Left<'_>,
    named: &BTreeMap<EntityId, Name>,
) -> Result<BTreeMap<EntityId, BTreeSet<Name>>, HeapFault> {
    let tag = left.values.tag();
    let mut owners: BTreeMap<EntityId, BTreeSet<Name>> = BTreeMap::new();
    for (id, owner) in named {
        let mut saving = Saving {
            heap: &mut *heap,
            left,
        };
        let site = saving.closure_site(*id)?;
        let Some(closure) = closure_at(left.document, &site) else {
            continue;
        };
        let [
            ..,
            Stmt::Let {
                name: result,
                value:
                    Rhs::Action(Action::Call {
                        callee: Callee::Value(callee),
                        ..
                    }),
            },
            Stmt::Return {
                value: Expr::Variable(returned),
            },
        ] = closure.body.as_slice()
        else {
            continue;
        };
        let reads = CodeReads::of(&closure.params, &closure.body);
        if result != returned
            || !reads.effects.is_empty()
            || !reads.declared.is_empty()
            || reads.free.len() != 1
            || !reads.free.contains(callee)
        {
            continue;
        }
        let value = match saving.closure_capture(*id, callee)? {
            Some(value) => Some(value),
            None if site.unit == Unit::Main => saving.unit_binding(&Unit::Main, callee)?,
            None => None,
        };
        if let Some(value) = value
            && let Some(helper) = closure_held(saving.heap, &value, tag)?
            && !named.contains_key(&helper)
        {
            owners.entry(helper).or_default().insert(owner.clone());
        }
    }
    Ok(owners)
}

/// The closure `id` saved under `name`, with the token its binding holds it
/// in frozen when a binding of that name does.
fn save_one(
    heap: &mut PagedHeap,
    left: &Left<'_>,
    name: &Name,
    id: EntityId,
    named: &BTreeMap<EntityId, Name>,
    owners: &BTreeMap<EntityId, BTreeSet<Name>>,
) -> Result<SavedFunction, SaveError> {
    let mut function = save_closure(heap, left, name, id, named, owners)?;
    let tag = left.values.tag();
    if let Some(value @ PageValue::Tuple(tuple)) = left.bindings.get(name)
        && let Some((held, at)) = token_closure(heap, value, tag)?
        && held == id
    {
        let values = members(heap, tuple)?;
        let mut saving = Saving {
            heap: &mut *heap,
            left,
        };
        function.token = Some(Token::frozen(freeze_token(
            &mut saving,
            &values,
            at,
            tag.unwrap_or_default(),
            named,
        )?));
    }
    Ok(function)
}

/// Whether the session still holds the function `needed` from an earlier
/// cell: one of a generated name, or one whose binding still holds it.
fn still_held(
    heap: &PagedHeap,
    left: &Left<'_>,
    held: &BTreeSet<Name>,
    needed: &Name,
) -> Result<bool, HeapFault> {
    if !held.contains(needed) {
        return Ok(false);
    }
    if needed.saved_index().is_some() {
        return Ok(true);
    }
    Ok(left
        .bindings
        .get(needed)
        .map(|value| function_reference(heap, value, left.values))
        .transpose()?
        .flatten()
        .as_ref()
        == Some(needed))
}

/// What a binding's value holds that decides whether it is carried.
enum Holds {
    /// A task, or a task's handle.
    Task,
    /// A closure where no reference can stand for it: as a key or a set
    /// member, behind a ref, or in an error's data.
    Keyed,
    /// These closures, each in a slot a reference can take.
    Closures(BTreeSet<EntityId>),
}

fn holds(
    heap: &PagedHeap,
    value: &PageValue,
    named: &BTreeMap<EntityId, Name>,
) -> Result<Holds, HeapFault> {
    let mut closures = BTreeSet::new();
    let mut keyed = false;
    let mut objects = BTreeSet::new();
    // Each value, and whether a reference cannot stand where it is.
    let mut pending = vec![(value.clone(), false)];
    while let Some((value, fixed)) = pending.pop() {
        match &value {
            PageValue::Task(_) | PageValue::Ref(Identity::Task(_)) => return Ok(Holds::Task),
            PageValue::Closure(id) if fixed || !named.contains_key(id) => keyed = true,
            PageValue::Closure(id) => {
                closures.insert(*id);
            }
            PageValue::Ref(Identity::Object(id)) => {
                if heap.kind(*id)? == Some(ObjectKind::Closure) {
                    keyed = true;
                } else if let Some(object) = heap.value_of(*id)? {
                    pending.push((object, true));
                }
            }
            PageValue::Tuple(tuple) => {
                pending.extend(members(heap, tuple)?.into_iter().map(|v| (v, fixed)));
            }
            PageValue::Error(error) => pending.push((heap.error_data(error)?, true)),
            PageValue::List(id)
            | PageValue::Map(id)
            | PageValue::Set(id)
            | PageValue::Record(id) => {
                if !objects.insert(*id) {
                    continue;
                }
                let set = matches!(value, PageValue::Set(_));
                heap.visit(*id, &mut |element| {
                    match element {
                        Element::Item(item) => pending.push((item.clone(), fixed)),
                        Element::Entry { key, value } => {
                            pending.push((key.clone(), true));
                            pending.push((value.clone(), fixed || set));
                        }
                        Element::Field { value, .. } => pending.push((value.clone(), fixed)),
                    }
                    ControlFlow::Continue(())
                })?;
            }
            _ => {}
        }
    }
    Ok(if keyed {
        Holds::Keyed
    } else {
        Holds::Closures(closures)
    })
}

/// `value` with each closure in it a reference to its saved function:
/// replaced where an object holds it, and answered when `value` itself is
/// one or a tuple holding one. `replaced` are the objects already done.
fn refer(
    heap: &mut PagedHeap,
    value: &PageValue,
    named: &BTreeMap<EntityId, Name>,
    replaced: &mut BTreeSet<EntityId>,
) -> Result<Option<PageValue>, HeapWriteError> {
    Ok(match value {
        PageValue::Closure(id) => {
            let name = named.get(id).ok_or_else(|| {
                HeapFault::Malformed("a carried closure is saved under no name".into())
            })?;
            Some(PageValue::Function(heap.text(name.as_str())?))
        }
        PageValue::Tuple(tuple) => {
            let mut values = members(heap, tuple)?;
            let mut changed = false;
            for value in &mut values {
                if let Some(referred) = refer(heap, value, named, replaced)? {
                    *value = referred;
                    changed = true;
                }
            }
            if changed {
                Some(PageValue::Tuple(heap.tuple(values)?))
            } else {
                None
            }
        }
        PageValue::List(id) => {
            if replaced.insert(*id) {
                refer_items(heap, *id, named, replaced)?;
            }
            None
        }
        PageValue::Map(id) | PageValue::Record(id) => {
            if replaced.insert(*id) {
                refer_entries(
                    heap,
                    *id,
                    matches!(value, PageValue::Record(_)),
                    named,
                    replaced,
                )?;
            }
            None
        }
        _ => None,
    })
}

/// Replaces the closures the list `list` holds, rewriting each card of its
/// rows that changed.
fn refer_items(
    heap: &mut PagedHeap,
    list: EntityId,
    named: &BTreeMap<EntityId, Name>,
    replaced: &mut BTreeSet<EntityId>,
) -> Result<(), HeapWriteError> {
    let mut items = Vec::new();
    heap.visit(list, &mut |element| {
        if let Element::Item(item) = element {
            items.push(item.clone());
        }
        ControlFlow::Continue(())
    })?;
    let mut cards = BTreeSet::new();
    for (at, item) in items.iter_mut().enumerate() {
        if let Some(referred) = refer(heap, item, named, replaced)? {
            *item = referred;
            cards.insert(at as u64 / CARD);
        }
    }
    if cards.is_empty() {
        return Ok(());
    }
    let len = items.len() as u64;
    let cards = cards
        .into_iter()
        .map(|card| {
            let start = (card * CARD) as usize;
            let end = ((card + 1) * CARD).min(len) as usize;
            (card, items[start..end].to_vec())
        })
        .collect();
    heap.store_cards(list, StoredCards::List { len, cards })
}

/// Replaces the closures a map or a record holds as values; a key keeps
/// its spelling and position.
fn refer_entries(
    heap: &mut PagedHeap,
    object: EntityId,
    record: bool,
    named: &BTreeMap<EntityId, Name>,
    replaced: &mut BTreeSet<EntityId>,
) -> Result<(), HeapWriteError> {
    let mut entries = Vec::new();
    heap.visit(object, &mut |element| {
        match element {
            Element::Entry { key, value } => entries.push((key.clone(), value.clone())),
            Element::Field { name, value } => {
                entries.push((PageValue::Text(name.clone()), value.clone()));
            }
            Element::Item(_) => {}
        }
        ControlFlow::Continue(())
    })?;
    for (key, value) in entries {
        let Some(referred) = refer(heap, &value, named, replaced)? else {
            continue;
        };
        match (&key, record) {
            (PageValue::Text(field), true) => {
                heap.record_write(object, FieldName::Text(field), referred)?;
            }
            _ => heap.map_write(object, key, referred)?,
        }
    }
    Ok(())
}

fn write_fault(error: HeapWriteError) -> HeapFault {
    match error {
        HeapWriteError::Fault(fault) => fault,
        other => HeapFault::Malformed(format!("a carried binding's reference: {other}")),
    }
}
