//! A sealed run's or session's code ids, rewritten (cutover contract §6):
//! the succession of a run onto another document or helper library.
//!
//! A run names code by content id in its state: the operation uses it
//! reads and waits on name their link, every site a library function's
//! [`FunctionId`], its live helpers' pins too, and a closure's or a
//! function's object, its head and a request's argument their
//! [`CallableId`]. A rewrite reads every one of them, refuses before it
//! writes anything when the correspondence names no image for one, and
//! then writes, copy-on-write, each page that holds one: the rows and
//! large chunks that do, the directory paths to them, the run's state and
//! a root over them. Every page the old root names stays as it was.
//!
//! A [`FunctionValue`] a request's argument holds, the closure a run
//! crossed into an effect, is rewritten like the rest: its callable, the
//! local functions its start convention names (its own link's), its
//! creation site, its token's members and every capture, nested function
//! values included. Its captions and signature origin are display
//! coordinates of a document, not code ids, and stay.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::heap::{HeapFault, LogicalBytes, PageRef};
use lash_kernel_doc::{
    CaptureValue, Correspondence, Datum, ErrorDatum, FunctionTarget, FunctionValue, StartConvention,
};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId};
use lash_kernel_value::sites::{EffectIdentity, Site, TaskIdentity, Unit};
use lash_kernel_value::tokens::{decode_body, encode_body};

use super::super::source::grant;
use super::super::tree::{Access, Tree};
use super::super::{CheckpointRoot, PageKind, PageSource, SessionRoot};
use super::chunked::{ChunkPage, IndexPage};
use super::continuation::{self, RunRecord};
use super::directory::{DirectoryRoot, Row, whole};
use super::ids::ObjectId;
use super::objects::{Head, Heads, Record, RunValue};
use super::root::HeapRoot;
use super::session;

/// The image of each code id a run names, identity entries included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunCorrespondence {
    pub links: BTreeMap<LinkId, LinkId>,
    /// A callable's image's link is its link's image.
    pub callables: BTreeMap<CallableId, CallableId>,
    pub functions: BTreeMap<FunctionId, FunctionId>,
    /// Where each node of the predecessor's own code went, when the
    /// succession moved or rewrote nodes: a site in one of its units is
    /// carried to its successor or refused; any other site keeps its path,
    /// its unit's function id mapped by `functions`.
    pub sites: Option<SiteMap>,
}

/// A document correspondence and the units of its base it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteMap {
    correspondence: Correspondence,
    units: BTreeSet<Unit>,
}

impl SiteMap {
    /// The site map `correspondence` states: its base's units are the
    /// units its entries leave from.
    pub fn new(correspondence: Correspondence) -> Self {
        let units = correspondence
            .entries()
            .iter()
            .map(|entry| entry.from.unit.clone())
            .collect();
        Self {
            correspondence,
            units,
        }
    }
}

/// A code id or site a run names and the correspondence has no image for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unmapped {
    Link(LinkId),
    Callable(CallableId),
    Function(FunctionId),
    /// A site of the predecessor's code whose node did not survive.
    Site(Site),
}

/// Why a run was not rewritten. Nothing was written when the fault is
/// [`Self::Unmapped`] or [`Self::Inconsistent`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RewriteFault {
    #[error("the run names {0:?}, which the correspondence does not map")]
    Unmapped(Unmapped),
    #[error("{0:?} maps to a callable whose link is not its link's image")]
    Inconsistent(CallableId),
    #[error(transparent)]
    Heap(#[from] HeapFault),
}

/// The code a sealed run names: the links its frames run, and every link,
/// callable and function id a rewrite needs an image for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveCode {
    /// The code each live frame runs: the link, the run's own for its
    /// declared code and a closure's for a frame running it, and the unit
    /// its site stands in, a helper's body as that helper's
    /// [`Unit::Library`].
    pub frames: BTreeSet<(LinkId, Unit)>,
    pub links: BTreeSet<LinkId>,
    pub callables: BTreeSet<CallableId>,
    pub functions: BTreeSet<FunctionId>,
    /// Every site the run stands at or names.
    pub sites: BTreeSet<Site>,
    /// The helpers the run's resume points reach, as the run pinned them.
    pub pins: BTreeSet<FunctionId>,
    /// The helpers code the run holds as a value may call, as it recorded
    /// them.
    pub held_code: BTreeSet<FunctionId>,
}

/// Either the ids met, or the images they are rewritten to.
enum Ids<'a> {
    Collect(&'a mut LiveCode),
    Map(&'a RunCorrespondence),
}

impl Ids<'_> {
    fn link(&mut self, link: &mut LinkId) -> Result<(), RewriteFault> {
        match self {
            Self::Collect(live) => {
                live.links.insert(*link);
            }
            Self::Map(map) => {
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
            Self::Map(map) => {
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
            Self::Map(map) => {
                let image = *map
                    .callables
                    .get(callable)
                    .ok_or(RewriteFault::Unmapped(Unmapped::Callable(*callable)))?;
                let link = *map
                    .links
                    .get(&callable.link)
                    .ok_or(RewriteFault::Unmapped(Unmapped::Link(callable.link)))?;
                if image.link != link {
                    return Err(RewriteFault::Inconsistent(*callable));
                }
                *callable = image;
            }
        }
        Ok(())
    }

    fn site(&mut self, site: &mut Site) -> Result<(), RewriteFault> {
        match self {
            Self::Collect(live) => {
                live.sites.insert(site.clone());
            }
            Self::Map(RunCorrespondence {
                sites: Some(map), ..
            }) if map.units.contains(&site.unit) => {
                *site = map
                    .correspondence
                    .successor(site)
                    .cloned()
                    .ok_or_else(|| RewriteFault::Unmapped(Unmapped::Site(site.clone())))?;
                return Ok(());
            }
            Self::Map(_) => {}
        }
        match &mut site.unit {
            Unit::Library(function) => self.function(function),
            Unit::Main | Unit::Function(_) => Ok(()),
        }
    }

    fn task(&mut self, task: &mut TaskIdentity) -> Result<(), RewriteFault> {
        if let TaskIdentity::Spawned(spawned) = task {
            self.task(Arc::make_mut(&mut spawned.parent))?;
            self.site(&mut spawned.site)?;
        }
        Ok(())
    }

    fn effect(&mut self, effect: &mut EffectIdentity) -> Result<(), RewriteFault> {
        self.task(&mut effect.task)?;
        self.site(&mut effect.site)?;
        for each in &mut effect.loops {
            self.site(&mut each.site)?;
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
            Datum::Error(error) => {
                let mut data = error.data().clone();
                self.datum(&mut data)?;
                if &data != error.data() {
                    **error = ErrorDatum::new(error.kind.clone(), error.message.clone(), data);
                }
            }
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
        if let Some(site) = &mut value.creation {
            self.site(site)?;
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
                self.link(&mut code.link)?;
                self.site(&mut code.site)?;
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
        for task in &mut record.tasks {
            self.task(&mut task.identity)?;
            for frame in &mut task.frames {
                self.site(&mut frame.unit.site)?;
                self.site(&mut frame.site)?;
            }
            for occurrence in &mut task.occurrences {
                self.site(&mut occurrence.site)?;
            }
            if let Some(origin) = &mut task.spawn_origin {
                self.site(origin)?;
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
            self.site(&mut request.origin)?;
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
    each: &mut dyn FnMut(ObjectId, Row) -> Result<(), RewriteFault>,
) -> Result<(), RewriteFault> {
    let mut found = Vec::new();
    tree(root).visit(access, &mut |key, row| {
        if let Some(id) = ObjectId::of_key(key) {
            found.push((id, row.clone()));
        }
        true
    })?;
    for (id, row) in found {
        each(id, row)?;
    }
    Ok(())
}

/// A visitor of a large object's chunks.
type Chunks<'a> = dyn FnMut(usize, &IndexPage, usize, ChunkPage) -> Result<(), RewriteFault> + 'a;

/// Calls `each` with every chunk page of large object `chunked`, its index
/// page's position among the parts and its own among the index's.
fn chunks(
    access: &Access<'_>,
    parts: &[super::chunked::Part],
    each: &mut Chunks<'_>,
) -> Result<(), RewriteFault> {
    for (at, part) in parts.iter().enumerate() {
        let answer = access.read(part.page)?;
        if answer.kind != PageKind::SEQUENCE_INDEX {
            return Err(corrupt(part.page, "an index part is not an index page").into());
        }
        let mut index: IndexPage =
            decode_body(&answer.tokens).map_err(|error| corrupt(part.page, &error.to_string()))?;
        for chunk in &mut index.0 {
            grant(&mut chunk.page, &answer.children)?;
        }
        for slot in 0..index.0.len() {
            let page = index.0[slot].page;
            let read = access.read(page)?;
            if read.kind != PageKind::SEQUENCE_LEAF {
                return Err(corrupt(page, "a chunk is not a leaf page").into());
            }
            let chunk: ChunkPage =
                decode_body(&read.tokens).map_err(|error| corrupt(page, &error.to_string()))?;
            each(at, &index, slot, chunk)?;
        }
    }
    Ok(())
}

/// Visits every code id the heap `heap` names, its directory's rows and
/// large chunks included, rewriting them when `ids` maps; gives the
/// rewritten heap root, or `heap` unchanged when collecting.
fn heap(access: &Access<'_>, heap: &HeapRoot, ids: &mut Ids<'_>) -> Result<HeapRoot, RewriteFault> {
    let mut changed: Vec<(ObjectId, Row)> = Vec::new();
    rows(access, heap.directory, &mut |id, mut row| {
        let before = row.clone();
        ids.row(&mut row)?;
        if let Record::Chunked(chunked) = &mut row.record {
            let mut indexes: BTreeMap<usize, IndexPage> = BTreeMap::new();
            chunks(access, &chunked.parts, &mut |at, index, slot, mut chunk| {
                let old = chunk.heads.clone();
                ids.heads(&mut chunk.heads)?;
                if chunk.heads != old {
                    let page =
                        access.seal(PageKind::SEQUENCE_LEAF, encode_body(&chunk), Vec::new())?;
                    indexes.entry(at).or_insert_with(|| index.clone()).0[slot].page = page;
                }
                Ok(())
            })?;
            for (at, index) in indexes {
                let children = index.0.iter().map(|chunk| chunk.page).collect();
                chunked.parts[at].page =
                    access.seal(PageKind::SEQUENCE_INDEX, encode_body(&index), children)?;
            }
        }
        if row != before {
            changed.push((id, row));
        }
        Ok(())
    })?;
    let mut out = heap.clone();
    if !changed.is_empty() {
        let mut directory = tree(heap.directory);
        for (id, row) in changed {
            directory.write(id.key().to_vec(), row, access)?;
        }
        directory.seal(access, &mut whole())?;
        out.directory = DirectoryRoot {
            page: directory.page(),
            rows: directory.len(),
        };
    }
    ids.heads(&mut out.heads)?;
    Ok(out)
}

/// The run sealed under `root`, its code ids rewritten by `map`: the
/// root's link, its state's and its heap's ids.
///
/// # Errors
///
/// [`RewriteFault::Unmapped`] or [`RewriteFault::Inconsistent`] for an id
/// `map` has no consistent image for, before any page is written; the
/// page port's faults.
pub fn rewrite(
    pages: &mut dyn PageSource,
    root: &CheckpointRoot,
    map: &RunCorrespondence,
) -> Result<CheckpointRoot, RewriteFault> {
    let access = Access::borrowed(pages);
    let mut record = continuation::read(&access, root.heap)?;
    let mut live = LiveCode::default();
    let mut collect = Ids::Collect(&mut live);
    collect.run(&mut record.clone())?;
    heap(&access, &record.heap, &mut collect)?;
    let mut out = root.clone();
    let mut ids = Ids::Map(map);
    ids.link(&mut out.link)?;
    check(&live, map)?;
    ids.run(&mut record)?;
    record.heap = heap(&access, &record.heap, &mut ids)?;
    out.heap = continuation::write(&access, &record)?;
    Ok(out)
}

/// The session `session` names, its code ids rewritten by `map`.
///
/// # Errors
///
/// As [`rewrite`]; [`HeapFault::Corrupt`] for a session not in the
/// bytecode core's form.
pub fn rewrite_session(
    pages: &mut dyn PageSource,
    session: &SessionRoot,
    map: &RunCorrespondence,
) -> Result<SessionRoot, RewriteFault> {
    let page = session.run_page().ok_or_else(|| {
        HeapFault::Malformed("the session is not in the bytecode core's form".into())
    })?;
    let access = Access::borrowed(pages);
    let mut carried = session::read(&access, page)?;
    let mut live = LiveCode::default();
    heap(&access, &carried.heap, &mut Ids::Collect(&mut live))?;
    check(&live, map)?;
    carried.heap = heap(&access, &carried.heap, &mut Ids::Map(map))?;
    Ok(SessionRoot::run(session::write(&access, &carried)?))
}

/// Refuses `live` an id `map` has no consistent image for.
fn check(live: &LiveCode, map: &RunCorrespondence) -> Result<(), RewriteFault> {
    let mut ids = Ids::Map(map);
    for link in &live.links {
        ids.link(&mut link.clone())?;
    }
    for callable in &live.callables {
        ids.callable(&mut callable.clone())?;
    }
    for function in &live.functions {
        ids.function(&mut function.clone())?;
    }
    for site in &live.sites {
        ids.site(&mut site.clone())?;
    }
    Ok(())
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
    let directory = tree(record.heap.directory);
    for frame in record.tasks.iter().flat_map(|task| &task.frames) {
        let closure = frame
            .closure
            .and_then(|at| record.heap.roots.get(at as usize))
            .and_then(RunValue::id);
        let link = match closure {
            None => root.link,
            Some(id) => match directory.get(&id.key(), &access)? {
                Some(Row {
                    record: Record::Closure { code, .. },
                    ..
                }) => code.link,
                _ => {
                    return Err(HeapFault::Malformed(format!(
                        "a frame runs object {}, which is no closure",
                        id.0
                    )));
                }
            },
        };
        live.frames.insert((link, frame.unit.site.unit.clone()));
    }
    live.pins = record.pins.iter().copied().collect();
    live.held_code = record.held_code.iter().copied().collect();
    Ok(live)
}
