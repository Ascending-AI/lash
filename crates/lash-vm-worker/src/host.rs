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
use lash_kernel_doc::{Datum, EffectName, ErrorDatum, Handle, Site, Timestamp};
use lash_kernel_state::paged::source::{READ_BATCH_BYTES, READ_BATCH_PAGES};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_vm_broker::kernel::pages::{answer_page, frame_page};
use lash_vm_client::wire::{self, EffectAnswer, EffectRead, ProjectionAnswer, ProjectionRead};
use lash_vm_client::{
    PoolError,
    ipc::{FrameSource, write_frame},
};
use lash_vm_protocol::page::{
    PageAuthority, PageCapability, PageEnvelope, PageExchangeId, PageRefusal, PageReply,
    PageRequest,
};
use lash_vm_protocol::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
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
    /// One host read: a frame out, a frame back.
    pub(crate) fn read(
        &self,
        kind: HostReadKind,
        request: EncodedPayload,
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
        self.send(&mut pipe, WorkerMessage::HostRead { id, kind, request })?;
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
    /// One page exchange, as a host read is one: a frame out, a frame back,
    /// under the run's fence.
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
        for phase in [WorkerPhase::Serializing, WorkerPhase::Responding] {
            self.send(
                &mut pipe,
                WorkerMessage::Progress {
                    phase,
                    cpu_nanos: cpu_nanos()?,
                    work: None,
                },
            )?;
        }
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
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Computing,
                cpu_nanos: cpu_nanos()?,
                work: None,
            },
        )?;
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
}

impl WireHost {
    #[expect(
        clippy::disallowed_methods,
        reason = "broken worker IPC terminates the worker and cannot become a catchable guest error"
    )]
    fn exchange<T: serde::de::DeserializeOwned>(
        &self,
        kind: HostReadKind,
        request: &impl serde::Serialize,
    ) -> T {
        wire::encode(PayloadKind::HostRead, request)
            .and_then(|request| self.wire.read(kind, request))
            .and_then(|answer| wire::decode(PayloadKind::HostAnswer, &answer))
            .unwrap_or_else(|error| {
                self.wire.refuse(&error);
                std::process::abort()
            })
    }
}

impl lash_kernel_vm::Host for WireHost {
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
        site: &Site,
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

const _: () = assert!(READ_BATCH_PAGES == PAGE_BATCH_PAGES && READ_BATCH_BYTES == PAGE_BATCH_BYTES);

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

/// The heap's page port on a worker (S3): reads and seals cross to the
/// parent's page service, over the run's host wire or in process, and every
/// page that comes back is re-hashed and re-framed here before the heap
/// decodes it.
///
/// Seals cross in batches. A sealed page waits in the port, readable from
/// it, and crosses with the pages sealed after it in one exchange, children
/// first, when the batch fills or the worker hands a page to the parent
/// ([`PagePort::flush`]). Until then the heap holds it under no grant; the
/// port answers for it with the grant the parent issued once it crossed.
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
    batch: SealBatch,
    /// The grant the parent issued for each page this port sealed.
    granted: HashMap<PageId, PageGrant>,
}

/// Sealed pages that have not crossed yet, in seal order.
#[derive(Default)]
struct SealBatch {
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
                batch: SealBatch::default(),
                granted: HashMap::new(),
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

    /// `page` as the parent's capability: the batch it waits in crosses
    /// first.
    ///
    /// # Errors
    ///
    /// The parent's refusal of that batch.
    pub(crate) fn capability(&self, page: PageRef) -> Result<PageCapability, HeapFault> {
        let mut port = self.0.borrow_mut();
        if port.batch.index.contains_key(&page.id) {
            port.flush()?;
        }
        Ok(capability_of(&port.resolved(page)))
    }

    /// `page` as the parent's capability, once every batch crossed.
    pub(crate) fn crossed(&self, page: PageRef) -> PageCapability {
        capability_of(&self.0.borrow().resolved(page))
    }
}

impl Port {
    fn exchange(&mut self, request: PageRequest) -> Result<PageReply, HeapFault> {
        self.traffic.exchanges += 1;
        (self.exchanger)(request)
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

    fn unsent(&self, page: &PageRef) -> Option<PageAnswer> {
        let at = *self.batch.index.get(&page.id)?;
        let mut answer = self.batch.pages[at].1.clone();
        answer.page = *page;
        Some(answer)
    }

    fn flush(&mut self) -> Result<(), HeapFault> {
        if self.batch.pages.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        // A child crosses with its capability unless it is sealed in this
        // batch or is a host node the heap holds no grant for (a checkpoint
        // root's release and owner binding): the parent decides those.
        let mut children = BTreeMap::new();
        for (_, own) in &batch.pages {
            for child in &own.children {
                let child = self.resolved(*child);
                if child.grant != PageGrant::NONE && !batch.index.contains_key(&child.id) {
                    children.insert(child.id, capability_of(&child));
                }
            }
        }
        let reply = self.exchange(PageRequest::Seal {
            pages: batch.pages.iter().map(|(page, _)| page.clone()).collect(),
            children: children.into_values().collect(),
        })?;
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
        let count = sealed.len() as u64;
        self.traffic.pages_sealed += count;
        crate::perf_witness::pages_sealed(count);
        for (capability, (page, own)) in sealed.iter().zip(batch.pages) {
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
        Ok(())
    }
}

impl PageSource for WirePageSource {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        if pages.len() > READ_BATCH_PAGES {
            return Err(HeapFault::Exhausted {
                resource: HostResource::ReadBatch,
            });
        }
        let port = &mut *self.port.borrow_mut();
        let mut fresh: HashMap<PageId, PageAnswer> = HashMap::new();
        let mut missing: Vec<PageRef> = Vec::new();
        for page in pages {
            if !missing.iter().any(|wanted| wanted.id == page.id)
                && !port.resident.pages.contains_key(&page.id)
                && !port.batch.index.contains_key(&page.id)
            {
                missing.push(port.resolved(*page));
            }
        }
        // The parent may answer a prefix that fits one frame; ask again for
        // the rest.
        while !missing.is_empty() {
            port.traffic.pages_requested += missing.len() as u64;
            crate::perf_witness::pages_loaded(missing.len() as u64);
            let reply = port.exchange(PageRequest::Read {
                pages: missing.iter().map(capability_of).collect(),
            })?;
            let read = match reply {
                PageReply::Read { pages } => pages,
                PageReply::Refused(refusal) => return Err(refused(refusal, missing[0].id)),
                PageReply::Sealed { .. } => return Err(breach("a read was answered as a seal")),
            };
            if read.is_empty() || read.len() > missing.len() {
                return Err(breach("a read reply answers no prefix of its request"));
            }
            for (wanted, got) in missing.iter().zip(&read) {
                if got.page.id != wanted.id.0 {
                    return Err(HeapFault::Corrupt {
                        page: wanted.id,
                        reason: "the parent answered another page".to_owned(),
                    });
                }
                let mut children = got.children.iter();
                let mut mismatch = false;
                let answer = answer_page(*wanted, &got.page.envelope, |child| {
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
                port.resident
                    .insert(answer.clone(), got.page.envelope.len() as u64);
                fresh.insert(wanted.id, answer);
            }
            missing.drain(..read.len());
        }
        pages
            .iter()
            .map(|page| {
                fresh
                    .get(&page.id)
                    .map(|answer| {
                        let mut answer = answer.clone();
                        answer.page = *page;
                        answer
                    })
                    .or_else(|| port.unsent(page))
                    .or_else(|| port.resident.get(page))
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect()
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        let node = frame_page(&page)?;
        let id = PageId(node.id.0);
        let port = &mut *self.port.borrow_mut();
        if port.batch.index.contains_key(&id) {
            return Ok(PageRef {
                id,
                grant: PageGrant::NONE,
            });
        }
        if let Some(grant) = port.granted.get(&id) {
            return Ok(PageRef { id, grant: *grant });
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
