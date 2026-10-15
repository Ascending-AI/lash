//! The machine's host, on the worker's side of the wire.
//!
//! The machine reads its host synchronously, in the middle of a slice, so
//! each read is one exchange under the run's own fence: the worker asks,
//! the parent answers, the slice runs on. What the run prints is kept until
//! the slice returns, and a run cancel is the flag the parent sent with the
//! slice.
use crate::worker::{Fences, cpu_nanos};
use lash_kernel_doc::{
    Datum, EffectIdentity, EffectName, ErrorDatum, Handle, Site, TaskIdentity, Timestamp,
};
use lash_vm_client::wire::{self, EffectAnswer, EffectRead, ProjectionAnswer, ProjectionRead};
use lash_vm_client::{
    PoolError,
    ipc::{FrameSource, write_frame},
};
use lash_vm_protocol::*;
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
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
    pub ran: lash_kernel_vm::RanDelta,
}

/// Each task's last entered document site since the run's previous frame.
/// The next run frame carries them, so observing a site costs no exchange:
/// a stretch of pure compute names the last site each task entered.
#[derive(Default)]
pub(crate) struct EnteredSites {
    last: BTreeMap<TaskIdentity, (u64, EffectIdentity)>,
    entries: u64,
}

impl EnteredSites {
    fn enter(&mut self, at: &EffectIdentity) {
        self.entries += 1;
        match self.last.get_mut(&at.task) {
            Some(last) => *last = (self.entries, at.clone()),
            None => {
                self.last
                    .insert(at.task.clone(), (self.entries, at.clone()));
            }
        }
    }

    /// The entries the next frame carries, in the order they were entered.
    fn take(&mut self) -> Vec<EffectIdentity> {
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
        if entered.is_empty() && ran.is_empty() {
            return Ok(None);
        }
        wire::encode(PayloadKind::Sites, &wire::Sites { entered, ran }).map(Some)
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

    fn enter_site(&mut self, at: &EffectIdentity) {
        if let Some(sites) = self.sites.as_mut() {
            sites.enter(at);
        }
    }

    fn observes_ran(&self) -> bool {
        self.sites.is_some()
    }

    fn ran(&mut self, delta: &lash_kernel_vm::RanDelta) {
        self.ran.extend_from_slice(delta);
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
