//! The machine's host, on the worker's side of the wire.
//!
//! The machine reads its host synchronously, in the middle of a slice, so
//! each read is one exchange under the run's own fence: the worker asks,
//! the parent answers, the slice runs on. What the run prints is kept until
//! the slice returns, and a run cancel is the flag the parent sent with the
//! slice.
//!
//! The persistent heap reads and seals its pages the same way
//! ([`WirePageSource`]): a page fault is one exchange with the parent's page
//! service, and the statement and native call that faulted wait on their
//! own stack until the page arrives. Nothing parks, retries or reruns.
use crate::worker::{Fences, cpu_nanos};
use lash_kernel_doc::heap::{
    HeapFault, HostResource, PAGE_GRANT_BYTES, PageGrant, PageId, PageRef,
};
use lash_kernel_doc::{Datum, EffectId, EffectName, ErrorDatum, Handle, TaskLabel, Timestamp};
use lash_kernel_state::paged::PagePortLimits;
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_vm_broker::kernel::pages::{answer_page, frame_page};
use lash_vm_client::wire::{self, EffectAnswer, EffectRead, ProjectionAnswer, ProjectionRead};
use lash_vm_client::{
    PoolError,
    ipc::{FrameSource, write_frame},
};
use lash_vm_protocol::page::{
    PageAuthority, PageCapability, PageEnvelope, PageExchangeId, PageRefusal, PageReply,
    PageRequest, ReadPage,
};
use lash_vm_protocol::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct Wire {
    pipe: Mutex<UnixStream>,
    /// The worker's one inbound source: this wire and its server read the
    /// same socket.
    inbound: Arc<Mutex<FrameSource>>,
    codec: FrameCodec,
    fences: Arc<Mutex<Fences>>,
    serialization: Duration,
    parent_wait: Duration,
    /// The most one request or answer may weigh.
    value_bound: u64,
}
impl Wire {
    pub fn new(
        pipe: UnixStream,
        inbound: Arc<Mutex<FrameSource>>,
        codec: FrameCodec,
        fences: Arc<Mutex<Fences>>,
        serialization: Duration,
        parent_wait: Duration,
        value_bound: u64,
    ) -> Self {
        Self {
            pipe: Mutex::new(pipe),
            inbound,
            codec,
            fences,
            serialization,
            parent_wait,
            value_bound,
        }
    }
    fn send(&self, pipe: &mut UnixStream, message: WorkerMessage) -> Result<(), PoolError> {
        let bytes = self
            .fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .encode(&self.codec, message)?;
        write_frame(pipe, &bytes, Instant::now() + self.serialization)
    }
    fn bound(&self, payload: &EncodedPayload) -> Result<(), PoolError> {
        if payload.0.len() as u64 > self.value_bound {
            return Err(InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::EffectValue {
                    size: payload.0.len() as u64,
                    bound: self.value_bound,
                },
            }
            .into());
        }
        Ok(())
    }
    /// One host read: a frame out, a frame back. The read carries the
    /// sites the run entered since its previous frame.
    pub(crate) fn read(
        &self,
        kind: HostReadKind,
        request: EncodedPayload,
        sites: Option<EncodedPayload>,
    ) -> Result<EncodedPayload, PoolError> {
        self.bound(&request)?;
        let mut pipe = self
            .pipe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = {
            let mut fences = self
                .fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = HostReadId(fences.next_read);
            fences.next_read += 1;
            id
        };
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Serializing,
                cpu_nanos: cpu_nanos()?,
                work: None,
            },
        )?;
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Responding,
                cpu_nanos: cpu_nanos()?,
                work: None,
            },
        )?;
        self.send(
            &mut pipe,
            WorkerMessage::HostRead {
                id,
                kind,
                request,
                sites,
            },
        )?;
        let bytes = self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .read_frame(&mut pipe, &self.codec, Instant::now() + self.parent_wait)?;
        let frame = self.codec.decode_parent(&bytes)?;
        self.fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .incoming
            .as_mut()
            .ok_or_else(|| PoolError::breach(SequenceFault::MissingLease))?
            .admit(&frame.header)
            .map_err(PoolError::breach)?;
        let ParentMessage::HostAnswer {
            id: answered,
            answer,
        } = frame.message
        else {
            return Err(PoolError::breach(SequenceFault::ReadOtherControl));
        };
        if answered != id {
            return Err(PoolError::breach(SequenceFault::WrongReadId));
        }
        self.bound(&answer)?;
        self.codec.check_payload(&answer.0)?;
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Computing,
                cpu_nanos: cpu_nanos()?,
                work: None,
            },
        )?;
        Ok(answer)
    }
    /// One page exchange: a frame out, a frame back, under the run's fence.
    /// It is asked mid-computation and computes on once answered, so it
    /// adds no progress phase of its own.
    pub(crate) fn pages(&self, request: PageRequest) -> Result<PageReply, PoolError> {
        let mut pipe = self
            .pipe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = {
            let mut fences = self
                .fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = PageExchangeId(fences.next_page);
            fences.next_page += 1;
            id
        };
        self.send(&mut pipe, WorkerMessage::Pages { id, request })?;
        let bytes = self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .read_frame(&mut pipe, &self.codec, Instant::now() + self.parent_wait)?;
        let frame = self.codec.decode_parent(&bytes)?;
        self.fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .incoming
            .as_mut()
            .ok_or_else(|| PoolError::breach(SequenceFault::MissingLease))?
            .admit(&frame.header)
            .map_err(PoolError::breach)?;
        let ParentMessage::Pages {
            id: answered,
            reply,
        } = frame.message
        else {
            return Err(PoolError::breach(SequenceFault::ReadOtherControl));
        };
        if answered != id {
            return Err(PoolError::breach(SequenceFault::WrongPageExchange));
        }
        Ok(reply)
    }
    pub fn refuse(&self, error: &PoolError) {
        let mut pipe = self
            .pipe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(message) = WorkerRefusal::testimony(error.clone().into_outcome()) {
            let _ = self.send(&mut pipe, message);
        }
    }
}

/// One slice's host.
pub(crate) struct WireHost {
    pub wire: Arc<Wire>,
    /// Whether the parent sent a run cancel with the slice.
    pub cancel: bool,
    /// What the slice printed, in order.
    pub printed: Vec<Datum>,
    /// The sites entered since the run's previous frame, when the parent
    /// observes them.
    pub sites: Option<EnteredSites>,
    pub ran: lash_kernel_vm::SiteCounts,
    /// The statement path since the run's previous frame, as document
    /// ordinals, when the parent records it.
    pub path: lash_kernel_vm::ExecutionPath,
}

/// Each task's last entered document site since the run's previous frame.
/// The next run frame carries them, so observing a site costs no exchange:
/// a stretch of pure compute names the last site each task entered.
#[derive(Default)]
pub(crate) struct EnteredSites {
    last: BTreeMap<TaskLabel, (u64, EffectId)>,
    entries: u64,
}

impl EnteredSites {
    fn enter(&mut self, at: &EffectId) {
        self.entries += 1;
        match self.last.get_mut(&at.task()) {
            Some(last) => *last = (self.entries, at.clone()),
            None => {
                self.last.insert(at.task(), (self.entries, at.clone()));
            }
        }
    }

    /// The entries the next frame carries, in the order they were entered.
    fn take(&mut self) -> Vec<EffectId> {
        let mut entered: Vec<_> = std::mem::take(&mut self.last).into_values().collect();
        entered.sort_unstable_by_key(|(order, _)| *order);
        entered.into_iter().map(|(_, at)| at).collect()
    }
}

impl WireHost {
    pub(crate) fn take_sites(&mut self) -> Result<Option<EncodedPayload>, PoolError> {
        let entered = self
            .sites
            .as_mut()
            .map(EnteredSites::take)
            .unwrap_or_default();
        let ran = std::mem::take(&mut self.ran);
        let path = std::mem::take(&mut self.path);
        if entered.is_empty() && ran.is_empty() && path.is_empty() {
            return Ok(None);
        }
        wire::encode(PayloadKind::Sites, &wire::Sites { entered, ran, path }).map(Some)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "broken worker IPC terminates the worker and cannot become a catchable guest error"
    )]
    fn exchange<T: serde::de::DeserializeOwned>(
        &mut self,
        kind: HostReadKind,
        request: &impl serde::Serialize,
    ) -> T {
        let sites = self.take_sites();
        wire::encode(PayloadKind::HostRead, request)
            .and_then(|request| self.wire.read(kind, request, sites?))
            .and_then(|answer| wire::decode(PayloadKind::HostAnswer, &answer))
            .unwrap_or_else(|error| {
                self.wire.refuse(&error);
                std::process::abort()
            })
    }
}

impl lash_kernel_vm::Host for WireHost {
    fn observes_sites(&self) -> bool {
        self.sites.is_some()
    }

    fn enter_site(&mut self, at: &EffectId) {
        if let Some(sites) = self.sites.as_mut() {
            sites.enter(at);
        }
    }

    fn observer(&mut self) -> Option<&mut dyn lash_kernel_vm::RunObserver> {
        self.sites.is_some().then_some(self)
    }

    fn clock(&mut self) -> Timestamp {
        self.exchange(HostReadKind::Clock, &())
    }

    fn random(&mut self) -> u64 {
        self.exchange(HostReadKind::Random, &())
    }

    fn read(&mut self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        let answer: ProjectionAnswer = self.exchange(
            HostReadKind::Projection,
            &ProjectionRead {
                handle: handle.clone(),
                request: request.clone(),
            },
        );
        answer
    }

    /// A read-bound `perform` is one more host read: the parent answers it
    /// from the effect's reader, under the run's fence, and the slice runs
    /// on. The parent's own failure never crosses: it stops the run there.
    fn perform(
        &mut self,
        effect: &EffectName,
        args: &[Datum],
        site: &lash_kernel_doc::PositionLabel,
    ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        let answer: EffectAnswer = self.exchange(
            HostReadKind::Effect,
            &EffectRead {
                effect: effect.clone(),
                args: args.to_vec(),
                site: site.clone(),
            },
        );
        Ok(answer)
    }

    fn print(&mut self, value: &Datum) {
        self.printed.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        self.cancel
    }
}

const _: () = assert!(
    PagePortLimits::STANDARD.pages == PAGE_BATCH_PAGES
        && PagePortLimits::STANDARD.bytes == PAGE_BATCH_BYTES
);

/// The authority word of a grant: a stage lease's id with its top bit set,
/// a root lease's without. Lease ids stay below 2^63.
const STAGE_BIT: u64 = 1 << 63;

/// The kernel's opaque grant for a parent capability (`D-GRANT`):
/// authority (8 bytes), activation epoch (8) and MAC (32).
pub(crate) fn grant_of(capability: &PageCapability) -> PageGrant {
    let authority = match capability.authority {
        PageAuthority::Root(id) => id & !STAGE_BIT,
        PageAuthority::Stage(id) => id | STAGE_BIT,
    };
    let mut grant = [0_u8; PAGE_GRANT_BYTES];
    grant[..8].copy_from_slice(&authority.to_le_bytes());
    grant[8..16].copy_from_slice(&capability.epoch.to_le_bytes());
    grant[16..].copy_from_slice(&capability.mac);
    PageGrant(grant)
}

/// The grant a start's carried session holds `link` under: the parent
/// names each of its pages by a capability.
pub(crate) fn link_grant(link: &lash_vm_client::wire::PageLink) -> Result<PageGrant, PoolError> {
    match link {
        lash_vm_client::wire::PageLink::Granted(capability) => Ok(grant_of(capability)),
        lash_vm_client::wire::PageLink::Carried(_) => Err(PoolError::payload(
            PayloadKind::Start,
            "a start's carried session names a page by no capability",
        )),
    }
}

/// The capability a page link's grant carries back to the parent.
pub(crate) fn capability_of(page: &PageRef) -> PageCapability {
    let word = |range: std::ops::Range<usize>| {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&page.grant.0[range]);
        u64::from_le_bytes(bytes)
    };
    let authority = word(0..8);
    let mut mac = [0_u8; 32];
    mac.copy_from_slice(&page.grant.0[16..]);
    PageCapability {
        page: page.id.0,
        authority: if authority & STAGE_BIT == 0 {
            PageAuthority::Root(authority)
        } else {
            PageAuthority::Stage(authority & !STAGE_BIT)
        },
        epoch: word(8..16),
        mac,
    }
}

/// The parent's refusal as the heap's infrastructure fault for `page`.
fn refused(refusal: PageRefusal, page: PageId) -> HeapFault {
    match refusal {
        PageRefusal::Unauthorized => HeapFault::Unauthorized { page },
        PageRefusal::LeaseLost => HeapFault::LeaseLost,
        PageRefusal::Missing => HeapFault::Missing { page },
        PageRefusal::Corrupt => HeapFault::Corrupt {
            page,
            reason: "the parent found the page corrupt".to_owned(),
        },
        PageRefusal::BatchTooLarge { .. } => HeapFault::Exhausted {
            resource: HostResource::ReadBatch,
        },
        PageRefusal::StagingExhausted { .. } => HeapFault::Exhausted {
            resource: HostResource::StagingBytes,
        },
        PageRefusal::Deadline => HeapFault::Exhausted {
            resource: HostResource::HostWork,
        },
        PageRefusal::StoreUnavailable => {
            HeapFault::Transport("the store is unavailable".to_owned())
        }
    }
}

fn transport(error: &PoolError) -> HeapFault {
    HeapFault::Transport(error.to_string())
}

fn breach(detail: &str) -> HeapFault {
    HeapFault::Transport(format!("the parent broke the page exchange: {detail}"))
}

/// The pages a worker keeps read, within its clean budget: the least
/// recently used are evicted first, and read again on demand. Eviction
/// changes latency only, never a value or a charge.
struct Resident {
    budget: u64,
    bytes: u64,
    tick: u64,
    pages: HashMap<PageId, (PageAnswer, u64, u64)>,
    order: BTreeMap<u64, PageId>,
}

impl Resident {
    fn get(&mut self, page: &PageRef) -> Option<PageAnswer> {
        self.tick += 1;
        let (answer, used, _) = self.pages.get_mut(&page.id)?;
        self.order.remove(used);
        *used = self.tick;
        self.order.insert(self.tick, page.id);
        let mut answer = answer.clone();
        answer.page = *page;
        Some(answer)
    }

    /// The grant `page` is held under, when it is held under one.
    fn grant(&self, page: &PageId) -> Option<PageGrant> {
        self.pages
            .get(page)
            .map(|(answer, _, _)| answer.page.grant)
            .filter(|grant| *grant != PageGrant::NONE)
    }

    fn insert(&mut self, answer: PageAnswer, size: u64) {
        if size > self.budget || self.pages.contains_key(&answer.page.id) {
            return;
        }
        while self.bytes + size > self.budget {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some((_, _, evicted)) = self.pages.remove(&oldest) {
                self.bytes -= evicted;
            }
        }
        self.tick += 1;
        self.order.insert(self.tick, answer.page.id);
        self.bytes += size;
        self.pages.insert(answer.page.id, (answer, self.tick, size));
    }
}

/// What a worker's page source exchanged, for measurement and the laws.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageTraffic {
    pub exchanges: u64,
    /// Pages named in read requests that crossed the wire.
    pub pages_requested: u64,
    pub pages_sealed: u64,
}

/// How a page source reaches the parent's page service: one exchange, the
/// heap's statement waiting in it.
pub(crate) type PageExchanger = Box<dyn FnMut(PageRequest) -> Result<PageReply, HeapFault>>;

/// The most pages one seal batch carries: the parent's batch bound.
const SEAL_BATCH_PAGES: usize = PAGE_BATCH_PAGES;
/// The most envelope bytes one seal batch carries: half the parent's batch
/// bound, so the batch and its children's capabilities fit one standard
/// frame.
const SEAL_BATCH_BYTES: u64 = PAGE_BATCH_BYTES / 2;
/// The most child capabilities one seal batch carries, well within one
/// standard frame's node bound.
const SEAL_BATCH_CHILDREN: usize = 512;
/// The fewest grants a port keeps before it drops those no checkpoint
/// reaches.
pub(crate) const GRANT_FLOOR: usize = 1024;

/// The heap's page port on a worker (S3): reads and seals cross to the
/// parent's page service, over the run's host wire or in process, and every
/// page that comes back is re-hashed and re-framed here before the heap
/// decodes it.
///
/// Seals cross in batches. A sealed page waits in the port, readable from
/// it, and crosses with the pages sealed after it, children first: in one
/// exchange when the batch fills ([`PagePort::flush`]), and otherwise with
/// the frame that hands the run to the parent ([`PagePort::take_batch`]),
/// whose grants come back with the run's next slice ([`PagePort::grant`]).
/// Until then the heap holds it under no grant; the port answers for it
/// with the grant the parent issued once it crossed.
#[cfg_attr(test, derive(Clone))]
pub(crate) struct WirePageSource {
    port: Rc<RefCell<Port>>,
}

/// The worker's hold on its run's page port: what it hands the parent
/// crosses first, under the grant the parent issued for it.
#[derive(Clone)]
pub(crate) struct PagePort(Rc<RefCell<Port>>);

struct Port {
    exchanger: PageExchanger,
    resident: Resident,
    traffic: PageTraffic,
    batch: Pending,
    /// The batch that crossed with the last frame that handed the run to
    /// the parent, until its grants come back.
    carried: Pending,
    /// The grant the parent issued for each page this port sealed that the
    /// machine may still name under none.
    granted: HashMap<PageId, PageGrant>,
    /// How many grants `granted` may hold before a checkpoint drops those
    /// its root no longer reaches.
    prune_at: usize,
}

/// Sealed pages that have not been granted yet, in seal order.
#[derive(Default)]
struct Pending {
    pages: Vec<(PageEnvelope, PageAnswer)>,
    index: HashMap<PageId, usize>,
    bytes: u64,
    children: usize,
}

impl WirePageSource {
    /// A source over `wire`, keeping at most `clean_bytes` of read pages.
    pub(crate) fn new(wire: Arc<Wire>, clean_bytes: u64) -> Self {
        Self::over(
            Box::new(move |request| wire.pages(request).map_err(|error| transport(&error))),
            clean_bytes,
        )
    }

    /// A source whose exchanges `exchanger` answers, keeping at most
    /// `clean_bytes` of read pages.
    pub(crate) fn over(exchanger: PageExchanger, clean_bytes: u64) -> Self {
        Self {
            port: Rc::new(RefCell::new(Port {
                exchanger,
                resident: Resident {
                    budget: clean_bytes,
                    bytes: 0,
                    tick: 0,
                    pages: HashMap::new(),
                    order: BTreeMap::new(),
                },
                traffic: PageTraffic::default(),
                batch: Pending::default(),
                carried: Pending::default(),
                granted: HashMap::new(),
                prune_at: GRANT_FLOOR,
            })),
        }
    }

    /// The worker's hold on this source's port.
    pub(crate) fn port(&self) -> PagePort {
        PagePort(self.port.clone())
    }

    #[cfg(test)]
    pub(crate) fn traffic(&self) -> PageTraffic {
        self.port.borrow().traffic
    }

    /// How many grants the port keeps for pages it sealed.
    #[cfg(test)]
    pub(crate) fn grants(&self) -> usize {
        self.port.borrow().granted.len()
    }
}

impl PagePort {
    /// Sends the pages sealed since the last batch crossed.
    ///
    /// # Errors
    ///
    /// The parent's refusal of the batch, as the heap's fault.
    pub(crate) fn flush(&self) -> Result<(), HeapFault> {
        self.0.borrow_mut().flush()
    }

    /// The pages sealed since the last batch crossed, to cross with the
    /// frame that hands the run to the parent (FIG-6288). They stay
    /// readable here until [`Self::grant`] takes their grants.
    pub(crate) fn take_batch(&self) -> Option<CarriedSeal> {
        self.0.borrow_mut().take_batch()
    }

    /// Takes the grants the parent issued for the batch that crossed last,
    /// in its order.
    ///
    /// # Errors
    ///
    /// A breach when they are not that batch's.
    pub(crate) fn grant(&self, sealed: Vec<PageCapability>) -> Result<(), HeapFault> {
        self.0.borrow_mut().grant(sealed)
    }

    /// Holds `pages`, which the run's start carried as a read of them would
    /// have answered them (FIG-6321), each checked as a read reply is.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Corrupt`] for a page that is not its envelope or whose
    /// capabilities are not its children's.
    pub(crate) fn warm(&self, pages: &[ReadPage]) -> Result<(), HeapFault> {
        let port = &mut *self.0.borrow_mut();
        // They come parents first: each is held under the capability its
        // parent's answer carried for it, a root under none until the heap
        // reads it under its own.
        let mut grants: HashMap<PageId, PageGrant> = HashMap::new();
        for got in pages {
            let id = PageId(got.page.id);
            let page = PageRef {
                id,
                grant: grants.get(&id).copied().unwrap_or(PageGrant::NONE),
            };
            port.accept(page, got)?;
            grants.extend(
                got.children
                    .iter()
                    .map(|capability| (PageId(capability.page), grant_of(capability))),
            );
        }
        Ok(())
    }

    /// The machine sealed a checkpoint rooted at `root` (FIG-6421): the
    /// port keeps the grants of the pages it sealed that the root, or a
    /// batch still to cross, reaches, and drops the rest.
    ///
    /// # Errors
    ///
    /// The parent's refusal of a read the walk needed, as the heap's fault.
    pub(crate) fn checkpointed(&self, root: PageRef) -> Result<(), HeapFault> {
        self.0.borrow_mut().checkpointed(root)
    }

    /// How a carried session names `page`: under the grant the parent
    /// issued, or as a page of the batch that crosses with it.
    pub(crate) fn link(&self, page: PageRef) -> lash_vm_client::wire::PageLink {
        let port = self.0.borrow();
        let resolved = port.resolved(page);
        if resolved.grant == PageGrant::NONE && port.carried.index.contains_key(&page.id) {
            lash_vm_client::wire::PageLink::Carried(page.id.0)
        } else {
            lash_vm_client::wire::PageLink::Granted(capability_of(&resolved))
        }
    }
}

/// A seal batch as it crosses with a frame.
pub(crate) type CarriedSeal = lash_vm_protocol::page::SealBatch;

impl Port {
    fn exchange(&mut self, request: PageRequest) -> Result<PageReply, HeapFault> {
        self.traffic.exchanges += 1;
        (self.exchanger)(request)
    }

    /// `got` as the answer for `wanted`, re-hashed and re-framed here, its
    /// children under the capabilities that came with it; held resident.
    fn accept(&mut self, wanted: PageRef, got: &ReadPage) -> Result<PageAnswer, HeapFault> {
        let mut children = got.children.iter();
        let mut mismatch = false;
        let answer = answer_page(wanted, &got.page.envelope, |child| {
            Ok(match children.next() {
                Some(capability) if capability.page == child.0 => grant_of(capability),
                _ => {
                    mismatch = true;
                    PageGrant::NONE
                }
            })
        })?;
        if mismatch || children.next().is_some() {
            return Err(HeapFault::Corrupt {
                page: wanted.id,
                reason: "its capabilities are not its envelope's children".to_owned(),
            });
        }
        self.resident
            .insert(answer.clone(), got.page.envelope.len() as u64);
        Ok(answer)
    }

    /// Reads `missing` from the parent in as many exchanges as its replies
    /// take, each answer checked and held resident.
    fn fetch(&mut self, mut missing: Vec<PageRef>) -> Vec<Result<PageAnswer, HeapFault>> {
        let mut fetched = Vec::with_capacity(missing.len());
        while !missing.is_empty() {
            self.traffic.pages_requested += missing.len() as u64;
            crate::perf_witness::pages_loaded(missing.len() as u64);
            let reply = self.exchange(PageRequest::Read {
                pages: missing.iter().map(capability_of).collect(),
            });
            let read = match reply {
                Ok(PageReply::Read { pages })
                    if !pages.is_empty() && pages.len() <= missing.len() =>
                {
                    pages
                }
                Ok(PageReply::Refused(refusal)) => {
                    fetched.extend(
                        missing
                            .iter()
                            .map(|page| Err(refused(refusal.clone(), page.id))),
                    );
                    break;
                }
                other => {
                    let fault = match other {
                        Err(fault) => fault,
                        _ => breach("a read reply answers no valid prefix of its request"),
                    };
                    fetched.extend(missing.iter().map(|_| Err(fault.clone())));
                    break;
                }
            };
            for (wanted, got) in missing.iter().zip(&read) {
                fetched.push(match got {
                    Err(refusal) => Err(refused(refusal.clone(), wanted.id)),
                    Ok(got) if got.page.id != wanted.id.0 => Err(HeapFault::Corrupt {
                        page: wanted.id,
                        reason: "the parent answered another page".to_owned(),
                    }),
                    Ok(got) => self.accept(*wanted, got),
                });
            }
            missing.drain(..read.len());
        }
        fetched
    }

    /// Drops the grants no live link can need (FIG-6421). A checkpoint root
    /// holds the whole resumable machine, so every page the machine can
    /// still name under no grant is one the root reaches, one a batch still
    /// to cross reaches, or one it seals later. The walk from those goes
    /// through the pages this port sealed and stops at any other page: a
    /// committed page names no staged one. It runs once the map has doubled
    /// since the walk before, so the map stays within twice the live pages
    /// (or [`GRANT_FLOOR`]) and each grant is walked a bounded number of
    /// times.
    fn checkpointed(&mut self, root: PageRef) -> Result<(), HeapFault> {
        if self.granted.len() <= self.prune_at {
            return Ok(());
        }
        let mut kept = HashMap::new();
        let mut seen = HashSet::new();
        let mut level: Vec<PageId> = std::iter::once(root.id)
            .chain(
                [&self.batch, &self.carried]
                    .into_iter()
                    .flat_map(|pending| pending.pages.iter().map(|(_, own)| own.page.id)),
            )
            .filter(|id| seen.insert(*id))
            .collect();
        while !level.is_empty() {
            let mut children = Vec::new();
            let mut unread = Vec::new();
            for id in level {
                let page = PageRef {
                    id,
                    grant: PageGrant::NONE,
                };
                if let Some(own) = self.unsent(&page) {
                    children.extend(own.children);
                } else if let Some(&grant) = self.granted.get(&id) {
                    kept.insert(id, grant);
                    match self.resident.pages.get(&id) {
                        Some((answer, _, _)) => children.extend_from_slice(&answer.children),
                        None => unread.push(PageRef { id, grant }),
                    }
                }
            }
            for batch in unread.chunks(PagePortLimits::STANDARD.pages) {
                for answer in self.fetch(batch.to_vec()) {
                    children.extend(answer?.children);
                }
            }
            level = children
                .into_iter()
                .map(|child| child.id)
                .filter(|id| seen.insert(*id))
                .collect();
        }
        self.granted = kept;
        self.prune_at = GRANT_FLOOR.max(2 * self.granted.len());
        Ok(())
    }

    /// `page` under the grant the parent issued for it, when this port
    /// sealed it and the heap holds it under none.
    fn resolved(&self, page: PageRef) -> PageRef {
        if page.grant != PageGrant::NONE {
            return page;
        }
        PageRef {
            id: page.id,
            grant: self
                .granted
                .get(&page.id)
                .copied()
                .unwrap_or(PageGrant::NONE),
        }
    }

    /// A page sealed here that the parent has not granted yet.
    fn unsent(&self, page: &PageRef) -> Option<PageAnswer> {
        let pending = [&self.batch, &self.carried]
            .into_iter()
            .find(|pending| pending.index.contains_key(&page.id))?;
        let mut answer = pending.pages[pending.index[&page.id]].1.clone();
        answer.page = *page;
        Some(answer)
    }

    fn ungranted(&self, page: &PageId) -> bool {
        self.batch.index.contains_key(page) || self.carried.index.contains_key(page)
    }

    /// `batch` as it crosses: its pages, and the capability of each child
    /// the heap holds under a grant. A child it holds under none is a page
    /// sealed earlier in the batch or carried by an earlier one, or a host
    /// node (a checkpoint root's release and owner binding): the parent
    /// decides those. A child the batch seals too still crosses with its
    /// grant: the parent stages the batch in seal order, and a committed
    /// page the heap read under a grant and seals again lands after the
    /// pages that already named it (FIG-6466).
    fn request(&self, batch: &Pending) -> CarriedSeal {
        // Counted as they cross, not when granted: a park's batch is
        // granted with the run's next slice, which a released worker
        // never receives.
        crate::perf_witness::pages_sealed(batch.pages.len() as u64);
        let mut children = BTreeMap::new();
        for (_, own) in &batch.pages {
            for child in &own.children {
                let child = self.resolved(*child);
                if child.grant != PageGrant::NONE {
                    children.insert(child.id, capability_of(&child));
                }
            }
        }
        CarriedSeal {
            pages: batch.pages.iter().map(|(page, _)| page.clone()).collect(),
            children: children.into_values().collect(),
        }
    }

    fn take_batch(&mut self) -> Option<CarriedSeal> {
        if self.batch.pages.is_empty() {
            return None;
        }
        let batch = std::mem::take(&mut self.batch);
        let request = self.request(&batch);
        // A batch carried before is granted with the run's next slice; a
        // second one in the same hand-over waits behind it in order.
        for (page, own) in batch.pages {
            self.carried
                .index
                .insert(own.page.id, self.carried.pages.len());
            self.carried.pages.push((page, own));
        }
        Some(request)
    }

    fn grant(&mut self, sealed: Vec<PageCapability>) -> Result<(), HeapFault> {
        if sealed.is_empty() && self.carried.pages.is_empty() {
            return Ok(());
        }
        let carried = std::mem::take(&mut self.carried);
        if sealed.len() != carried.pages.len()
            || sealed
                .iter()
                .zip(&carried.pages)
                .any(|(capability, (page, _))| capability.page != page.id)
        {
            return Err(breach("a run's grants are not its carried batch's"));
        }
        self.admitted(&sealed, carried.pages);
        Ok(())
    }

    /// Holds each of `pages` under the grant the parent issued for it.
    fn admitted(&mut self, sealed: &[PageCapability], pages: Vec<(PageEnvelope, PageAnswer)>) {
        let count = sealed.len() as u64;
        self.traffic.pages_sealed += count;
        for (capability, (page, own)) in sealed.iter().zip(pages) {
            let grant = grant_of(capability);
            self.granted.insert(own.page.id, grant);
            self.resident.insert(
                PageAnswer {
                    page: PageRef {
                        id: own.page.id,
                        grant,
                    },
                    ..own
                },
                page.envelope.len() as u64,
            );
        }
    }

    fn flush(&mut self) -> Result<(), HeapFault> {
        if self.batch.pages.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let request = self.request(&batch);
        let reply = self.exchange(request.into_request())?;
        let sealed = match reply {
            PageReply::Sealed { pages }
                if pages.len() == batch.pages.len()
                    && pages
                        .iter()
                        .zip(&batch.pages)
                        .all(|(sealed, (page, _))| sealed.page == page.id) =>
            {
                pages
            }
            // A child that crossed with no capability is the one the parent
            // could refuse to authorize: the refusal names it.
            PageReply::Refused(refusal) => {
                let first = batch.pages[0].1.page.id;
                let blamed = batch
                    .pages
                    .iter()
                    .flat_map(|(_, own)| &own.children)
                    .find(|child| {
                        self.resolved(**child).grant == PageGrant::NONE
                            && !batch.index.contains_key(&child.id)
                    })
                    .filter(|_| refusal == PageRefusal::Unauthorized)
                    .map_or(first, |child| child.id);
                return Err(refused(refusal, blamed));
            }
            _ => return Err(breach("a seal was not answered with its pages")),
        };
        self.admitted(&sealed, batch.pages);
        Ok(())
    }
}

impl PageSource for WirePageSource {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        if pages.len() > PagePortLimits::STANDARD.pages {
            return Err(HeapFault::Exhausted {
                resource: HostResource::ReadBatch,
            });
        }
        let port = &mut *self.port.borrow_mut();
        let mut missing: Vec<PageRef> = Vec::new();
        for page in pages {
            let resolved = port.resolved(*page);
            if !missing.contains(&resolved)
                && !port.resident.pages.contains_key(&page.id)
                && !port.ungranted(&page.id)
            {
                missing.push(resolved);
            }
        }
        let fetched = port.fetch(missing.clone());
        let fresh: HashMap<_, _> = missing.into_iter().zip(fetched).collect();
        Ok(pages
            .iter()
            .map(|page| {
                if let Some(answer) = fresh.get(&port.resolved(*page)) {
                    return answer.clone().map(|mut answer| {
                        answer.page = *page;
                        answer
                    });
                }
                port.unsent(page)
                    .or_else(|| port.resident.get(page))
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        let node = frame_page(&page)?;
        let id = PageId(node.id.0);
        let port = &mut *self.port.borrow_mut();
        if port.ungranted(&id) {
            return Ok(PageRef {
                id,
                grant: PageGrant::NONE,
            });
        }
        if let Some(grant) = port.granted.get(&id) {
            return Ok(PageRef { id, grant: *grant });
        }
        // A page the run read is committed or staged already, under the
        // grant it was read with: the parent need not see it again.
        if let Some(grant) = port.resident.grant(&id) {
            return Ok(PageRef { id, grant });
        }
        let grants: HashMap<PageId, PageGrant> = page
            .children
            .iter()
            .map(|child| (child.id, child.grant))
            .collect();
        // Our own framing read back: its children, each with the grant the
        // heap holds for it, and the answer a later read of it returns.
        let own = answer_page(
            PageRef {
                id,
                grant: PageGrant::NONE,
            },
            &node.bytes,
            |child| Ok(grants.get(&child).copied().unwrap_or(PageGrant::NONE)),
        )?;
        let size = node.bytes.len() as u64;
        let batch = &port.batch;
        if !batch.pages.is_empty()
            && (batch.pages.len() == SEAL_BATCH_PAGES
                || batch.bytes + size > SEAL_BATCH_BYTES
                || batch.children + own.children.len() > SEAL_BATCH_CHILDREN)
        {
            port.flush()?;
        }
        let batch = &mut port.batch;
        batch.index.insert(id, batch.pages.len());
        batch.bytes += size;
        batch.children += own.children.len();
        batch.pages.push((
            PageEnvelope {
                id: node.id.0,
                envelope: node.bytes.to_vec(),
            },
            own,
        ));
        Ok(PageRef {
            id,
            grant: PageGrant::NONE,
        })
    }
}

impl lash_kernel_vm::RunObserver for WireHost {
    fn counts(&mut self, counts: &lash_kernel_vm::SiteCounts) {
        self.ran.extend_from_slice(counts);
    }
}
