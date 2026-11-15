//! A session cell's end on the bytecode core (`K-SES-004`): the functions
//! the bindings its carry drops reach, saved by identity on the sealed run
//! heap.
//!
//! On the core a variable `main` declares is a session binding read by
//! name, and a closure's captures are the cells of the locals it shares,
//! so nothing is frozen: a saved function is its heap object, kept as a
//! session root with its cells, and a binding that holds saved functions
//! only where one can stand carries as it is.
//!
//! - Every closure or function object a dropped binding reaches, and
//!   every one a capture of those reaches, is a candidate, found before
//!   any is judged.
//! - A candidate is saved when a cell's document defines its code, no
//!   operation its code reaches ends the turn, and no capture reaches a
//!   task. A binding holding it, itself or in its token, names it; any
//!   other takes the next generated name.
//! - A saved function whose capture reaches a refused candidate, or whose
//!   code reads a session binding that is not carried, is refused in
//!   turn, until none is.
//! - A dropped binding carries when every function it reaches is saved
//!   and stands where a saved function can: not as a key or set member,
//!   behind a ref or in an error.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::snapshot::CaptureRefusal;
use lash_kernel_doc::snapshot::CodeReads;
use lash_kernel_doc::{Document, FunctionCatalog, FunctionRepresentation};
use lash_kernel_state::paged::run::chunked::{Members, each_members};
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::ObjectId;
use lash_kernel_state::paged::run::objects::{ObjectKind, Record, RunValue};
use lash_kernel_value::sites::{Site, Unit};
use lash_kernel_value::{CallableId, LinkId, Name};

use super::{HeldAt, NotSaved, SaveError, check_controls};
use crate::Written;

/// A core cell's end as its save reads it.
#[derive(Clone, Copy)]
pub struct Left<'a> {
    pub document: &'a Document,
    pub link: LinkId,
    pub documents: &'a BTreeMap<LinkId, Document>,
    /// Every binding the cell leaves, carried or dropped, by name.
    pub bindings: &'a BTreeMap<Name, RunValue>,
    /// The bindings the carry drops, in name order.
    pub not_carried: &'a [Name],
    pub controls: &'a BTreeMap<lash_kernel_value::OperationUse, BTreeSet<crate::EffectControl>>,
    /// What the source wrote of each function the document makes, by the
    /// site of its closure ([`crate::Lowered::written`]).
    pub written: &'a BTreeMap<Site, Written>,
    /// How the dialect holds a function as a value.
    pub values: &'a FunctionRepresentation,
    /// The library the cell ran against.
    pub catalog: &'a dyn FunctionCatalog,
}

/// A function saved on the core: its object, which carries its code and
/// cells, and what its description records.
#[derive(Clone, Debug)]
pub struct SavedFunction {
    pub name: Name,
    pub link: LinkId,
    pub callable: CallableId,
    /// The closure or function object, a session root.
    pub object: ObjectId,
    pub document: lash_kernel_doc::IdentifiedDocument,
    /// The session bindings its code reads by name.
    pub references: BTreeSet<Name>,
    pub written: Option<Written>,
    /// The tag of the token a binding holds it in, when one does.
    pub token: Option<String>,
    pub held: Option<HeldAt>,
}

/// What a core cell's end saved.
#[derive(Debug, Default)]
pub struct Saved {
    pub functions: BTreeMap<Name, SavedFunction>,
    /// The dropped bindings that carry as they are.
    pub carried: BTreeSet<Name>,
    pub refused: BTreeMap<Name, NotSaved>,
}

/// One candidate: its object, the dropped binding it was first reached
/// from and the path from that binding.
struct Found {
    object: ObjectId,
    root: usize,
    path: String,
}

/// What one candidate's code is and needs.
struct Candidate {
    found: Found,
    callable: CallableId,
    /// A closure's expression coordinate; none for a function object.
    site: Option<Site>,
    /// The candidates its captures reach.
    captured: BTreeSet<ObjectId>,
    /// The session bindings its code reads.
    reads: BTreeSet<Name>,
    /// A capture that reaches a task.
    task: Option<Name>,
}

/// Saves what it can of the functions `left.not_carried` reach on the
/// sealed heap `directory`; `held` names the functions the session
/// already holds.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Malformed`] for a value naming an
/// object the directory does not hold.
pub fn save(
    directory: &Directory,
    left: Left<'_>,
    held: &BTreeSet<Name>,
) -> Result<Saved, HeapFault> {
    let mut walk = Walk {
        directory,
        seen: BTreeSet::new(),
    };
    let mut found = Vec::new();
    for (root, name) in left.not_carried.iter().enumerate() {
        if let Some(value) = left.bindings.get(name) {
            walk.reach(value, root, name.to_string(), &mut found)?;
        }
    }
    // Captures are edges too: every candidate is found before any is
    // judged, so neither mutual capture nor walk order decides one.
    let mut candidates = BTreeMap::<ObjectId, Candidate>::new();
    let mut order = Vec::new();
    let mut queue = std::collections::VecDeque::from(found);
    while let Some(current) = queue.pop_front() {
        let mut more = Vec::new();
        let candidate = walk.candidate(&left, current, &mut more)?;
        queue.extend(more);
        order.push(candidate.found.object);
        candidates.insert(candidate.found.object, candidate);
    }

    let mut names = BTreeMap::<ObjectId, Name>::new();
    for (name, value) in left.bindings {
        if let Some(object) = walk.held(value, left.values.tag())? {
            names.entry(object).or_insert_with(|| name.clone());
        }
    }
    let first = held
        .iter()
        .filter_map(Name::saved_index)
        .max()
        .map_or(0, |index| index + 1);
    let mut saved = Saved::default();
    let mut refusals = BTreeMap::<ObjectId, NotSaved>::new();
    let mut by_object = BTreeMap::<ObjectId, Name>::new();
    for (next, object) in (first..).zip(&order) {
        let candidate = &candidates[object];
        let name = names
            .entry(*object)
            .or_insert_with(|| Name::saved(next))
            .clone();
        match admit(&left, &walk, &name, candidate)? {
            Ok(function) => {
                by_object.insert(*object, name.clone());
                saved.functions.insert(name, function);
            }
            Err(why) => {
                refusals.insert(*object, why.clone());
                saved.refused.insert(name, why);
            }
        }
    }

    // A refusal invalidates every saved function that needs what it
    // refused, until none does.
    loop {
        let mut holds = BTreeMap::new();
        for name in left.not_carried {
            let classified = match left.bindings.get(name) {
                Some(value) => walk.holds(value, &by_object, &refusals)?,
                None => Holds::Task,
            };
            holds.insert(name.clone(), classified);
        }
        let mut needs = Vec::new();
        for (object, name) in &by_object {
            let candidate = &candidates[object];
            let missing = candidate
                .captured
                .iter()
                .find(|captured| !by_object.contains_key(captured))
                .map(|captured| {
                    names
                        .get(captured)
                        .cloned()
                        .unwrap_or_else(|| Name::new(format!("#{}", captured.0)))
                })
                .or_else(|| {
                    candidate
                        .reads
                        .iter()
                        .find(|read| {
                            holds
                                .get(*read)
                                .is_some_and(|holds| !matches!(holds, Holds::Carries))
                        })
                        .cloned()
                });
            if let Some(function) = missing {
                needs.push((*object, name.clone(), function));
            }
        }
        if needs.is_empty() {
            for (name, classified) in holds {
                match classified {
                    Holds::Carries => {
                        saved.carried.insert(name);
                    }
                    Holds::Task => {
                        saved.refused.insert(name, NotSaved::Task);
                    }
                    Holds::Keyed => {
                        saved.refused.insert(name, NotSaved::Keyed);
                    }
                    Holds::Refused(why) => {
                        saved.refused.insert(name, why);
                    }
                }
            }
            return Ok(saved);
        }
        for (object, name, function) in needs {
            by_object.remove(&object);
            saved.functions.remove(&name);
            let why = NotSaved::Needs { function };
            refusals.insert(object, why.clone());
            saved.refused.insert(name, why);
        }
    }
}

/// The saved function `candidate` is under `name`, or why it is refused.
fn admit(
    left: &Left<'_>,
    walk: &Walk<'_>,
    name: &Name,
    candidate: &Candidate,
) -> Result<Result<SavedFunction, NotSaved>, HeapFault> {
    let callable = candidate.callable;
    let defining = if callable.link == left.link {
        Some(left.document)
    } else {
        left.documents.get(&callable.link)
    };
    // A helper's Link supplies its operation context, not ownership of
    // its code: only a definition a cell owns is saved.
    let Some(document) = defining.filter(|document| {
        document.main.is_some() && document.functions.contains_key(&callable.function)
    }) else {
        return Ok(Err(NotSaved::MadeByLibrary));
    };
    if let Some(name) = &candidate.task {
        return Ok(Err(NotSaved::Capture {
            name: name.clone(),
            why: CaptureRefusal::Task,
        }));
    }
    match check_controls(left.controls, left.catalog, callable, document) {
        Ok(()) => {}
        Err(SaveError::Refused(why)) => return Ok(Err(why)),
        Err(SaveError::Heap(fault)) => return Err(fault),
    }
    let token = match (left.values, left.bindings.get(name)) {
        (FunctionRepresentation::Token { tag, .. }, Some(value))
            if walk.token(value, tag)? == Some(candidate.found.object) =>
        {
            Some(tag.clone())
        }
        _ => None,
    };
    let held = HeldAt {
        binding: left.not_carried[candidate.found.root].clone(),
        path: candidate.found.path.clone(),
    };
    Ok(Ok(SavedFunction {
        name: name.clone(),
        link: callable.link,
        callable,
        object: candidate.found.object,
        document: lash_kernel_doc::IdentifiedDocument::new(document.clone()),
        references: candidate.reads.clone(),
        written: candidate
            .site
            .as_ref()
            .and_then(|site| left.written.get(site))
            .cloned(),
        token,
        held: Some(held),
    }))
}

/// How a dropped binding stands once the save has judged its functions.
enum Holds {
    Carries,
    Task,
    Keyed,
    Refused(NotSaved),
}

/// Reads of the sealed heap.
struct Walk<'a> {
    directory: &'a Directory,
    /// The objects the candidate walk has entered.
    seen: BTreeSet<ObjectId>,
}

impl Walk<'_> {
    fn record(&self, id: ObjectId) -> Result<Record, HeapFault> {
        Ok(self
            .directory
            .get(id)?
            .ok_or_else(|| HeapFault::Malformed(format!("object {} has no row", id.0)))?
            .record)
    }

    /// The members of the container `record`, each with whether it is a
    /// key, a set member or an error's: a position no saved function can
    /// stand in.
    fn members(&self, record: &Record) -> Result<Vec<(RunValue, bool)>, HeapFault> {
        Ok(match record {
            Record::List(values) | Record::Tuple(values) => {
                values.iter().map(|value| (value.clone(), false)).collect()
            }
            Record::Set(values) => values.iter().map(|value| (value.clone(), true)).collect(),
            Record::Map(pairs) | Record::Record(pairs) => pairs
                .iter()
                .flat_map(|(key, value)| [(key.clone(), true), (value.clone(), false)])
                .collect(),
            Record::Error {
                kind,
                message,
                data,
            } => [kind, message, data]
                .into_iter()
                .map(|value| (value.clone(), true))
                .collect(),
            Record::Cell(value) => vec![(value.clone(), false)],
            Record::Chunked(chunked) => {
                let set = chunked.kind == ObjectKind::Set;
                let mut members = Vec::new();
                each_members(self.directory.access(), chunked, &mut |chunk| match chunk {
                    Members::List(values) => {
                        members.extend(values.iter().map(|value| (value.clone(), false)));
                    }
                    Members::Table(entries) => {
                        for entry in entries {
                            members.push((entry.key.clone(), true));
                            members.push((entry.value.clone(), set));
                        }
                    }
                })?;
                members
            }
            _ => Vec::new(),
        })
    }

    /// Queues every closure or function object `value` reaches, by its
    /// path from root binding `root`, that no walk has found yet.
    fn reach(
        &mut self,
        value: &RunValue,
        root: usize,
        path: String,
        found: &mut Vec<Found>,
    ) -> Result<(), HeapFault> {
        let mut pending = vec![(value.clone(), path)];
        while let Some((value, path)) = pending.pop() {
            let RunValue::Object(kind, id) = value else {
                continue;
            };
            if !self.seen.insert(id) {
                continue;
            }
            match kind {
                ObjectKind::Closure | ObjectKind::Function => found.push(Found {
                    object: id,
                    root,
                    path,
                }),
                ObjectKind::List
                | ObjectKind::Tuple
                | ObjectKind::Set
                | ObjectKind::Map
                | ObjectKind::Record => {
                    let record = self.record(id)?;
                    let keyed = matches!(record, Record::Map(_) | Record::Record(_))
                        || matches!(&record, Record::Chunked(chunked) if chunked.kind != ObjectKind::List);
                    let members = self.members(&record)?;
                    let children = members.into_iter().enumerate().map(|(at, (value, key))| {
                        let path = match (keyed, key) {
                            (false, _) => format!("{path}[{at}]"),
                            (true, true) => format!("{path}[key]"),
                            (true, false) => format!("{path}[value]"),
                        };
                        (value, path)
                    });
                    let children: Vec<_> = children.collect();
                    pending.extend(children.into_iter().rev());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The candidate `found` is: its code, what its captures reach, queued
    /// on `queue`, and the session bindings its code reads.
    fn candidate(
        &mut self,
        left: &Left<'_>,
        found: Found,
        queue: &mut Vec<Found>,
    ) -> Result<Candidate, HeapFault> {
        let (callable, site, cells) = match self.record(found.object)? {
            Record::Closure { code, captures } => (
                code.callable,
                Some(code.site),
                code.capture_names
                    .into_iter()
                    .zip(captures)
                    .collect::<Vec<_>>(),
            ),
            Record::Function(callable) => (callable, None, Vec::new()),
            _ => {
                return Err(HeapFault::Malformed(format!(
                    "object {} is named a function but is none",
                    found.object.0
                )));
            }
        };
        let mut candidate = Candidate {
            callable,
            site,
            captured: BTreeSet::new(),
            reads: BTreeSet::new(),
            task: None,
            found,
        };
        for (name, cell) in cells {
            let Record::Cell(value) = self.record(cell)? else {
                return Err(HeapFault::Malformed(format!(
                    "capture {name} of object {} is no cell",
                    candidate.found.object.0
                )));
            };
            let task = self.data(&value, &mut |id| {
                candidate.captured.insert(id);
            })?;
            if task && candidate.task.is_none() {
                candidate.task = Some(name.clone());
            }
            self.reach(
                &value,
                candidate.found.root,
                format!("{}.captures.{name}", candidate.found.path),
                queue,
            )?;
        }
        // A closure `main` made reads the cell's bindings by name.
        if let Some(Site {
            unit: Unit::Main, ..
        }) = &candidate.site
            && callable.link == left.link
            && let Some(definition) = left.document.functions.get(&callable.function)
            && let Some(body) = definition.body()
        {
            let params = definition
                .signature
                .params
                .iter()
                .map(|param| param.name.clone())
                .collect::<Vec<_>>();
            candidate.reads = CodeReads::of(&params, &body.block, left.catalog)
                .free
                .into_iter()
                .filter(|name| left.bindings.contains_key(name))
                .collect();
        }
        Ok(candidate)
    }

    /// Calls `each` with every closure or function object `value` reaches
    /// through data, not into their captures; gives whether it reaches a
    /// task.
    fn data(&self, value: &RunValue, each: &mut dyn FnMut(ObjectId)) -> Result<bool, HeapFault> {
        let mut task = false;
        let mut pending = vec![value.clone()];
        let mut seen = BTreeSet::new();
        while let Some(value) = pending.pop() {
            let id = match value {
                RunValue::Task(_) | RunValue::TaskRef(_) => {
                    task = true;
                    continue;
                }
                RunValue::Object(ObjectKind::Closure | ObjectKind::Function, id) => {
                    each(id);
                    continue;
                }
                RunValue::Object(_, id) | RunValue::Ref(id) => id,
                _ => continue,
            };
            if !seen.insert(id) {
                continue;
            }
            let record = self.record(id)?;
            if let Record::Closure { .. } | Record::Function(_) = record {
                each(id);
                continue;
            }
            pending.extend(self.members(&record)?.into_iter().map(|(value, _)| value));
        }
        Ok(task)
    }

    /// The function object `value` is, itself or in its token tagged `tag`.
    fn held(&self, value: &RunValue, tag: Option<&str>) -> Result<Option<ObjectId>, HeapFault> {
        match (value, tag) {
            (RunValue::Object(ObjectKind::Closure | ObjectKind::Function, id), _) => Ok(Some(*id)),
            (_, Some(tag)) => self.token(value, tag),
            _ => Ok(None),
        }
    }

    /// The one function object the token `value` holds, when `value` is a
    /// tuple whose first member is the text `tag`.
    fn token(&self, value: &RunValue, tag: &str) -> Result<Option<ObjectId>, HeapFault> {
        let RunValue::Object(ObjectKind::Tuple, id) = value else {
            return Ok(None);
        };
        let Record::Tuple(members) = self.record(*id)? else {
            return Ok(None);
        };
        let tagged = match members.first() {
            Some(RunValue::Text(text)) => &**text == tag,
            Some(RunValue::Object(ObjectKind::LongText, text)) => {
                matches!(self.record(*text)?, Record::LongText(text) if &*text == tag)
            }
            _ => false,
        };
        if !tagged {
            return Ok(None);
        }
        let mut functions = members.iter().filter_map(|member| match member {
            RunValue::Object(ObjectKind::Closure | ObjectKind::Function, id) => Some(*id),
            _ => None,
        });
        let first = functions.next();
        Ok(if functions.next().is_none() {
            first
        } else {
            None
        })
    }

    /// How the dropped binding `value` stands, `saved` the saved functions'
    /// objects and `refusals` why each refused one was.
    fn holds(
        &self,
        value: &RunValue,
        saved: &BTreeMap<ObjectId, Name>,
        refusals: &BTreeMap<ObjectId, NotSaved>,
    ) -> Result<Holds, HeapFault> {
        let mut keyed = false;
        let mut seen = BTreeSet::new();
        // Each value, and whether a function cannot stand where it is.
        let mut pending = vec![(value.clone(), false)];
        while let Some((value, fixed)) = pending.pop() {
            match value {
                RunValue::Task(_) | RunValue::TaskRef(_) => return Ok(Holds::Task),
                RunValue::Object(ObjectKind::Closure | ObjectKind::Function, id) => {
                    if let Some(why) = refusals.get(&id) {
                        return Ok(Holds::Refused(why.clone()));
                    }
                    if fixed || !saved.contains_key(&id) {
                        keyed = true;
                    }
                }
                RunValue::Object(kind, id) => {
                    if (kind.mutable() || matches!(kind, ObjectKind::Tuple | ObjectKind::Error))
                        && seen.insert((id, fixed))
                    {
                        let record = self.record(id)?;
                        pending.extend(
                            self.members(&record)?
                                .into_iter()
                                .map(|(value, key)| (value, fixed || key)),
                        );
                    }
                }
                RunValue::Ref(id) => {
                    let record = self.record(id)?;
                    if let Record::Closure { .. } | Record::Function(_) = record {
                        keyed = true;
                    } else if seen.insert((id, true)) {
                        pending.extend(
                            self.members(&record)?
                                .into_iter()
                                .map(|(value, _)| (value, true)),
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(if keyed { Holds::Keyed } else { Holds::Carries })
    }
}
