//! A sealed run's code ids, rewritten (cutover contract §6): the
//! succession of a run onto another document or helper library.
//!
//! A run's active code moves (`K-VER-005`): the run's own Link, its
//! frames' and occurrence counters' units, the operation uses it reads and
//! waits on, and every site, a library function's [`FunctionId`] and its
//! live helpers' pins included, take their images. A callable the run
//! holds does not: a closure's or a function's object, its head, a
//! request's argument and a [`FunctionValue`]'s callable and captures keep
//! their exact defining Link and code, and so do a held closure's
//! expression site and every frame, occurrence counter and site of its
//! body. The correspondence declares those Links; a held callable of
//! another refuses. A rewrite therefore never changes the heap: it reads
//! every id first, refuses before it writes anything, and then writes the
//! run's state and a root over the same heap.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::heap::{HeapFault, LogicalBytes, PageRef};
use lash_kernel_doc::{
    CaptureValue, Correspondence, Datum, FunctionTarget, FunctionValue, StartConvention,
};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId};
use lash_kernel_value::sites::{EffectIdentity, Site, TaskIdentity, Unit};
use lash_kernel_value::tokens::decode_body;

use super::super::source::grant;
use super::super::tree::{Access, Tree};
use super::super::{CheckpointRoot, PageKind, PageSource};
use super::chunked::{ChunkPage, IndexPage, Members};
use super::continuation::{self, RunRecord};
use super::directory::{DirectoryRoot, Row};
use super::ids::ObjectId;
use super::objects::{Head, Heads, ObjectKind, Record, RunValue};
use super::root::HeapRoot;

/// The image of each code id a run's active code names, identity entries
/// included, and the Links of the code it holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunCorrespondence {
    /// The image of each Link active code names: the run's own, its
    /// frames' and occurrence counters' units' and its operation uses'.
    pub links: BTreeMap<LinkId, LinkId>,
    pub functions: BTreeMap<FunctionId, FunctionId>,
    /// The Links of the callables the run holds (`K-VER-005`): each keeps
    /// its exact defining Link and code.
    pub retained: BTreeSet<LinkId>,
    /// Where each node of the predecessor's own code went, when the
    /// succession moved or rewrote nodes: a site of its main or a declared
    /// function is carried to its successor or refused; a library unit's
    /// site keeps its path, its function id mapped by `functions`.
    pub sites: Option<SiteMap>,
}

/// A document correspondence: where each node of its base went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteMap(Correspondence);

impl SiteMap {
    /// The site map `correspondence` states.
    pub fn new(correspondence: Correspondence) -> Self {
        Self(correspondence)
    }
}

/// A code id or site a run names and the correspondence has no image for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unmapped {
    Link(LinkId),
    Function(FunctionId),
    /// A site of the predecessor's code whose node did not survive, and
    /// what of the run holds it.
    Site {
        site: Site,
        held: &'static str,
    },
}

/// Why a run was not rewritten. Nothing was written when the fault is
/// [`Self::Unmapped`] or [`Self::NotRetained`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RewriteFault {
    #[error("the run names {0:?}, which the correspondence does not map")]
    Unmapped(Unmapped),
    #[error("the run holds {0:?}, whose Link the correspondence does not retain")]
    NotRetained(CallableId),
    #[error(transparent)]
    Heap(#[from] HeapFault),
}

/// The code a sealed run names: the links its frames run, every link and
/// function id its active code names, and the code it holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveCode {
    /// The code each live frame runs: its unit's link, the defining link
    /// of the code it runs, and the unit its site stands in, a helper's
    /// body as that helper's [`Unit::Library`].
    pub frames: BTreeSet<(LinkId, Unit)>,
    /// Every Link the run names; the successor holds each.
    pub links: BTreeSet<LinkId>,
    /// Every callable the run holds; a rewrite retains their Links.
    pub callables: BTreeSet<CallableId>,
    /// Each closure the run holds: a site in its function's unit is its
    /// body's.
    pub closures: BTreeSet<CallableId>,
    pub functions: BTreeSet<FunctionId>,
    /// Every site the run stands at or names.
    pub sites: BTreeSet<Site>,
    /// The helpers the run's resume points reach, as the run pinned them.
    pub pins: BTreeSet<FunctionId>,
    /// The helpers code the run holds as a value may call, as it recorded
    /// them.
    pub held_code: BTreeSet<FunctionId>,
    /// The helper that built each package value the run's heap holds: the
    /// `__lash_built` of every record with a well-formed `__lash_brand`.
    pub built: BTreeSet<FunctionId>,
}

/// Either the ids met, or the images they are rewritten to, with the
/// closures the run holds, whose code keeps its identity.
enum Ids<'a> {
    Collect(&'a mut LiveCode),
    Map {
        map: &'a RunCorrespondence,
        held: &'a BTreeSet<CallableId>,
    },
}

impl Ids<'_> {
    fn link(&mut self, link: &mut LinkId) -> Result<(), RewriteFault> {
        match self {
            Self::Collect(live) => {
                live.links.insert(*link);
            }
            Self::Map { map, .. } => {
                *link = *map
                    .links
                    .get(link)
                    .ok_or(RewriteFault::Unmapped(Unmapped::Link(*link)))?;
            }
        }
        Ok(())
    }

    fn function(&mut self, function: &mut FunctionId) -> Result<(), RewriteFault> {
        match self {
            Self::Collect(live) => {
                live.functions.insert(*function);
            }
            Self::Map { map, .. } => {
                *function = *map
                    .functions
                    .get(function)
                    .ok_or(RewriteFault::Unmapped(Unmapped::Function(*function)))?;
            }
        }
        Ok(())
    }

    fn callable(&mut self, callable: &mut CallableId) -> Result<(), RewriteFault> {
        match self {
            Self::Collect(live) => {
                live.links.insert(callable.link);
                live.callables.insert(*callable);
            }
            Self::Map { map, .. } => {
                if !map.retained.contains(&callable.link) {
                    return Err(RewriteFault::NotRetained(*callable));
                }
            }
        }
        Ok(())
    }

    /// Whether `site`, of code of `link` when that is known, is a held
    /// closure's body's, which keeps its identity.
    fn held(&self, link: Option<LinkId>, site: &Site) -> bool {
        match self {
            Self::Collect(_) => false,
            Self::Map { held, .. } => match site.unit {
                Unit::Library(function) => held.iter().any(|closure| {
                    closure.function == function && link.is_none_or(|link| link == closure.link)
                }),
                Unit::Main | Unit::Function(_) => false,
            },
        }
    }

    /// `site`, which `held` names the holder of.
    fn site(&mut self, site: &mut Site, held: &'static str) -> Result<(), RewriteFault> {
        self.site_of(None, site, held)
    }

    /// `site`, of code of `link` when that is known.
    fn site_of(
        &mut self,
        link: Option<LinkId>,
        site: &mut Site,
        held: &'static str,
    ) -> Result<(), RewriteFault> {
        if self.held(link, site) {
            return Ok(());
        }
        match self {
            Self::Collect(live) => {
                live.sites.insert(site.clone());
            }
            Self::Map {
                map:
                    RunCorrespondence {
                        sites: Some(SiteMap(map)),
                        ..
                    },
                ..
            } if !matches!(site.unit, Unit::Library(_)) => {
                *site = map.successor(site).cloned().ok_or_else(|| {
                    RewriteFault::Unmapped(Unmapped::Site {
                        site: site.clone(),
                        held,
                    })
                })?;
                return Ok(());
            }
            Self::Map { .. } => {}
        }
        match &mut site.unit {
            Unit::Library(function) => self.function(function),
            Unit::Main | Unit::Function(_) => Ok(()),
        }
    }

    fn task(&mut self, task: &mut TaskIdentity) -> Result<(), RewriteFault> {
        if let TaskIdentity::Spawned(spawned) = task {
            self.task(Arc::make_mut(&mut spawned.parent))?;
            self.site(&mut spawned.site, "task spawn")?;
        }
        Ok(())
    }

    fn effect(&mut self, effect: &mut EffectIdentity) -> Result<(), RewriteFault> {
        self.task(&mut effect.task)?;
        self.site(&mut effect.site, "wait")?;
        for each in &mut effect.loops {
            self.site(&mut each.site, "wait loop")?;
        }
        Ok(())
    }

    fn datum(&mut self, datum: &mut Datum) -> Result<(), RewriteFault> {
        match datum {
            Datum::Function(callable) => self.callable(callable)?,
            Datum::Tuple(members) | Datum::List(members) | Datum::Set(members) => {
                for member in members {
                    self.datum(member)?;
                }
            }
            Datum::Map(pairs) => {
                for (key, member) in pairs {
                    self.datum(key)?;
                    self.datum(member)?;
                }
            }
            Datum::Record(fields) => {
                for (_, member) in fields {
                    self.datum(member)?;
                }
            }
            Datum::Error(error) => self.datum(&mut error.data().clone())?,
            Datum::Null
            | Datum::Absent
            | Datum::Bool(_)
            | Datum::Int(_)
            | Datum::Float(_)
            | Datum::Number(_)
            | Datum::Text(_)
            | Datum::Bytes(_)
            | Datum::Timestamp(_)
            | Datum::Handle(_) => {}
            Datum::FunctionValue(value) => self.function_value(value)?,
        }
        Ok(())
    }

    /// A held function value: its callable, the local functions its start
    /// convention names (its own Link's), its creation site, its token's
    /// members and every capture, nested function values included.
    fn function_value(&mut self, value: &mut FunctionValue) -> Result<(), RewriteFault> {
        let link = value.callable.link;
        self.callable(&mut value.callable)?;
        match &mut value.convention {
            StartConvention::Direct | StartConvention::PyArgsList { .. } => {}
            StartConvention::TsDirect { await_fn: target }
            | StartConvention::PyDirect { cleanup: target } => self.target(link, target)?,
            StartConvention::TsArgsList { await_fn, same_fn } => {
                self.target(link, await_fn)?;
                self.target(link, same_fn)?;
            }
        }
        if let (Self::Collect(live), Some(site)) = (&mut *self, &value.creation) {
            live.sites.insert(site.clone());
        }
        for member in value.token.iter_mut().flatten() {
            self.datum(member)?;
        }
        for capture in value.captures.values_mut() {
            self.capture(capture)?;
        }
        Ok(())
    }

    /// A start convention's local function: a callable of `link`.
    fn target(&mut self, link: LinkId, target: &mut FunctionTarget) -> Result<(), RewriteFault> {
        if let FunctionTarget::Local(function) = target {
            let mut callable = CallableId {
                link,
                function: *function,
            };
            self.callable(&mut callable)?;
            *function = callable.function;
        }
        Ok(())
    }

    fn capture(&mut self, capture: &mut CaptureValue) -> Result<(), RewriteFault> {
        match capture {
            CaptureValue::Data(datum) => self.datum(datum),
            CaptureValue::Function(value) => self.function_value(value),
            CaptureValue::Reference(_) => Ok(()),
            CaptureValue::Tuple(members)
            | CaptureValue::List(members)
            | CaptureValue::Set(members) => members
                .iter_mut()
                .try_for_each(|member| self.capture(member)),
            CaptureValue::Map(pairs) => pairs.iter_mut().try_for_each(|(key, member)| {
                self.capture(key)?;
                self.capture(member)
            }),
            CaptureValue::Record(fields) => fields
                .iter_mut()
                .try_for_each(|(_, member)| self.capture(member)),
            CaptureValue::Error { data, .. } => self.capture(data),
        }
    }

    fn heads(&mut self, heads: &mut Heads) -> Result<(), RewriteFault> {
        for (_, head) in &mut heads.0 {
            if let Head::Function(callable) = head {
                self.callable(callable)?;
            }
        }
        Ok(())
    }

    /// A row's record and heads; a large object's chunks are apart.
    fn row(&mut self, row: &mut Row) -> Result<(), RewriteFault> {
        match &mut row.record {
            Record::Closure { code, .. } => {
                self.callable(&mut code.callable)?;
                if let Self::Collect(live) = self {
                    live.links.insert(code.link);
                    live.sites.insert(code.site.clone());
                    live.closures.insert(code.callable);
                }
            }
            Record::Function(callable) => self.callable(callable)?,
            _ => {}
        }
        self.heads(&mut row.heads)
    }

    /// The run's state beside its heap; [`heap`] does the heap's.
    fn run(&mut self, record: &mut RunRecord) -> Result<(), RewriteFault> {
        for read in &mut record.reads {
            self.link(&mut read.link)?;
        }
        // Every task's spawn origin answers before any frame does.
        for origin in record
            .tasks
            .iter_mut()
            .filter_map(|task| task.spawn_origin.as_mut())
        {
            self.site(origin, "spawn origin")?;
        }
        for task in &mut record.tasks {
            self.task(&mut task.identity)?;
            for frame in &mut task.frames {
                // A frame running a held closure runs its retained code.
                if frame.closure.is_none() || matches!(self, Self::Collect(_)) {
                    self.link(&mut frame.unit.link)?;
                    self.site(&mut frame.unit.site, "frame compilation origin")?;
                    let unit = frame.site.unit.clone();
                    self.site(&mut frame.site, "frame point")?;
                    // A slot is named by the path of its declaring site in
                    // the frame's unit; it moves with the unit's sites.
                    for slot in &mut frame.slots {
                        let old = Site::new(unit.clone(), std::mem::take(&mut slot.site));
                        let mut site = old.clone();
                        self.site(&mut site, "variable declaration")?;
                        if site.unit != frame.site.unit {
                            return Err(RewriteFault::Unmapped(Unmapped::Site {
                                site: old,
                                held: "variable declaration",
                            }));
                        }
                        slot.site = site.path;
                    }
                }
            }
            for occurrence in &mut task.occurrences {
                if !self.held(Some(occurrence.link), &occurrence.site) {
                    self.link(&mut occurrence.link)?;
                }
                self.site_of(
                    Some(occurrence.link),
                    &mut occurrence.site,
                    "action occurrence",
                )?;
            }
        }
        for request in record
            .waits
            .iter_mut()
            .filter_map(|wait| wait.request.as_mut())
        {
            self.effect(&mut request.identity)?;
            self.link(&mut request.use_.link)?;
            for arg in &mut request.args {
                self.datum(arg)?;
            }
            self.site(&mut request.origin, "request origin")?;
        }
        for function in record.pins.iter_mut().chain(&mut record.held_code) {
            self.function(function)?;
        }
        Ok(())
    }
}

fn corrupt(page: PageRef, reason: &str) -> HeapFault {
    HeapFault::Corrupt {
        page: page.id,
        reason: reason.into(),
    }
}

/// The directory `root` names, over `access`.
fn tree(root: DirectoryRoot) -> Tree<Row> {
    Tree::open(root.page, root.rows, LogicalBytes(0), false)
}

/// Calls `each` with every row of the directory `root` names.
fn rows(
    access: &Access<'_>,
    root: DirectoryRoot,
    each: &mut dyn FnMut(Row) -> Result<(), RewriteFault>,
) -> Result<(), RewriteFault> {
    let mut found = Vec::new();
    tree(root).visit(access, &mut |_, row| {
        found.push(row.clone());
        true
    })?;
    found.into_iter().try_for_each(each)
}

/// Calls `each` with every chunk page of the large object of `parts`.
fn chunks(
    access: &Access<'_>,
    parts: &[super::chunked::Part],
    each: &mut dyn FnMut(ChunkPage) -> Result<(), RewriteFault>,
) -> Result<(), RewriteFault> {
    for part in parts {
        let answer = access.read(part.page)?;
        if answer.kind != PageKind::SEQUENCE_INDEX {
            return Err(corrupt(part.page, "an index part is not an index page").into());
        }
        let mut index: IndexPage =
            decode_body(&answer.tokens).map_err(|error| corrupt(part.page, &error.to_string()))?;
        for chunk in &mut index.0 {
            grant(&mut chunk.page, &answer.children)?;
        }
        for chunk in &index.0 {
            let read = access.read(chunk.page)?;
            if read.kind != PageKind::SEQUENCE_LEAF {
                return Err(corrupt(chunk.page, "a chunk is not a leaf page").into());
            }
            each(
                decode_body(&read.tokens)
                    .map_err(|error| corrupt(chunk.page, &error.to_string()))?,
            )?;
        }
    }
    Ok(())
}

/// Visits every code id the heap `heap` holds: its directory's rows, their
/// heads and their large chunks', and its root's heads. The heap holds
/// only held code, which a rewrite keeps, so the visit never changes it.
fn heap(access: &Access<'_>, heap: &HeapRoot, ids: &mut Ids<'_>) -> Result<(), RewriteFault> {
    rows(access, heap.directory, &mut |mut row| {
        ids.row(&mut row)?;
        if let Record::Chunked(chunked) = &row.record {
            chunks(access, &chunked.parts, &mut |mut chunk| {
                ids.heads(&mut chunk.heads)
            })?;
        }
        Ok(())
    })?;
    ids.heads(&mut heap.heads.clone())
}

/// The run sealed under `root`, its active code's ids rewritten by
/// `map`: the root's link and its state's ids, over the same heap.
///
/// # Errors
///
/// [`RewriteFault::Unmapped`] for an id of active code `map` has no image
/// for, [`RewriteFault::NotRetained`] for a held callable whose Link `map`
/// does not retain, before any page is written; the page port's faults.
pub fn rewrite(
    pages: &mut dyn PageSource,
    root: &CheckpointRoot,
    map: &RunCorrespondence,
) -> Result<CheckpointRoot, RewriteFault> {
    let access = Access::borrowed(pages);
    let mut record = continuation::read(&access, root.heap)?;
    let mut live = LiveCode::default();
    heap(&access, &record.heap, &mut Ids::Collect(&mut live))?;
    let mut ids = Ids::Map {
        map,
        held: &live.closures,
    };
    heap(&access, &record.heap, &mut ids)?;
    let mut out = root.clone();
    ids.link(&mut out.link)?;
    ids.run(&mut record)?;
    out.heap = continuation::write(&access, &record)?;
    Ok(out)
}

/// An effect identity of the run whose code is `live`, rewritten by `map`
/// as the run's own requests are: one a held closure's body issued keeps
/// its sites.
///
/// # Errors
///
/// [`RewriteFault::Unmapped`] for an id `map` has no image for.
pub fn rewrite_effect(
    effect: &mut EffectIdentity,
    map: &RunCorrespondence,
    live: &LiveCode,
) -> Result<(), RewriteFault> {
    Ids::Map {
        map,
        held: &live.closures,
    }
    .effect(effect)
}

/// The code the run sealed under `root` names.
///
/// # Errors
///
/// The page port's faults.
pub fn live_code(pages: &mut dyn PageSource, root: &CheckpointRoot) -> Result<LiveCode, HeapFault> {
    let access = Access::borrowed(pages);
    let record = continuation::read(&access, root.heap)?;
    let mut live = LiveCode::default();
    live.links.insert(root.link);
    let mut collect = Ids::Collect(&mut live);
    let fault = |fault: RewriteFault| match fault {
        RewriteFault::Heap(fault) => fault,
        other => HeapFault::Malformed(other.to_string()),
    };
    collect.run(&mut record.clone()).map_err(fault)?;
    heap(&access, &record.heap, &mut collect).map_err(fault)?;
    for frame in record.tasks.iter().flat_map(|task| &task.frames) {
        live.frames
            .insert((frame.unit.link, frame.unit.site.unit.clone()));
    }
    live.pins = record.pins.iter().copied().collect();
    live.held_code = record.held_code.iter().copied().collect();
    live.built = built(&access, &record.heap).map_err(fault)?;
    Ok(live)
}

/// The helper that built each package value the heap `heap` holds: the
/// 32-byte `__lash_built` of every record, inline or large, whose
/// `__lash_brand` is a tuple of two texts.
fn built(access: &Access<'_>, heap: &HeapRoot) -> Result<BTreeSet<FunctionId>, RewriteFault> {
    let mut objects = BTreeMap::new();
    tree(heap.directory).visit(access, &mut |key, row| {
        if let Some(id) = ObjectId::of_key(key) {
            objects.insert(id, row.record.clone());
        }
        true
    })?;
    let field = |key: &RunValue, name: &str| matches!(key, RunValue::Text(text) if &**text == name);
    let text = |value: &RunValue| {
        matches!(
            value,
            RunValue::Text(_) | RunValue::Object(ObjectKind::LongText, _)
        )
    };
    let object = |value: &RunValue| value.id().and_then(|id| objects.get(&id));
    let mut built = BTreeSet::new();
    for record in objects.values() {
        let mut fields = Vec::new();
        match record {
            Record::Record(pairs) => fields.extend(pairs.iter().cloned()),
            Record::Chunked(chunked) if chunked.kind == ObjectKind::Record => {
                chunks(access, &chunked.parts, &mut |chunk| {
                    if let Members::Table(entries) = chunk.members {
                        fields.extend(entries.into_iter().map(|entry| (entry.key, entry.value)));
                    }
                    Ok(())
                })?;
            }
            _ => continue,
        }
        let branded = fields.iter().any(|(key, value)| {
            field(key, "__lash_brand")
                && matches!(object(value), Some(Record::Tuple(parts)) if parts.len() == 2 && parts.iter().all(text))
        });
        if !branded {
            continue;
        }
        for (key, value) in &fields {
            if field(key, "__lash_built")
                && let Some(Record::Bytes(bytes)) = object(value)
                && let Ok(digest) = <[u8; 32]>::try_from(&bytes[..])
            {
                built.insert(FunctionId::from_bytes(digest));
            }
        }
    }
    Ok(built)
}
