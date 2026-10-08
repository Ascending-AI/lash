//! Pure, demand-driven projection reads under the run's real IPC fence.
//!
//! A projection value is plain data naming its resource (ADR 0132 §9), so a
//! read sends the resource itself to the parent, which answers through the
//! provider registered for its type. A batch of reads of one resource is one
//! frame.
use crate::worker::{Fences, cpu_nanos};
use lash_vm_client::{
    PoolError, ProjectionAnswer, ProjectionRead,
    ipc::{FrameSource, write_frame},
};
use lash_vm_protocol::*;
use lashlang::{
    ProjectedReadRequest, ProjectedReadResponse, ProjectionReadError, ProjectionReader, ResourceRef,
};
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
}
impl Wire {
    pub fn new(
        pipe: UnixStream,
        inbound: Arc<Mutex<FrameSource>>,
        codec: FrameCodec,
        fences: Arc<Mutex<Fences>>,
        serialization: Duration,
        parent_wait: Duration,
    ) -> Self {
        Self {
            pipe: Mutex::new(pipe),
            inbound,
            codec,
            fences,
            serialization,
            parent_wait,
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
    /// Every request of `resource` in one frame, answered in order.
    pub(crate) fn read(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<ProjectionAnswer, PoolError> {
        let mut pipe = self
            .pipe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = {
            let mut fences = self
                .fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = EffectRequestId(fences.next_effect);
            fences.next_effect += 1;
            id
        };
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Serializing,
                cpu_nanos: cpu_nanos()?,
            },
        )?;
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Responding,
                cpu_nanos: cpu_nanos()?,
            },
        )?;
        self.send(
            &mut pipe,
            WorkerMessage::EffectRequest(EffectRequest {
                id,
                kind: EffectKind::ProjectionRead,
                payload: EncodedPayload(
                    rmp_serde::to_vec_named(&ProjectionRead {
                        resource: resource.clone(),
                        requests,
                    })
                    .map_err(|error| PoolError::payload(PayloadKind::ProjectionRead, error))?,
                ),
            }),
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
        let ParentMessage::EffectResponse(EffectResponse {
            id: response_id,
            outcome: EffectOutcome::Value(payload),
        }) = frame.message
        else {
            return Err(PoolError::breach(SequenceFault::ProjectionOtherControl));
        };
        if response_id != id {
            return Err(PoolError::breach(SequenceFault::WrongRequestId));
        }
        self.codec.check_payload(&payload.0)?;
        let answer: ProjectionAnswer = rmp_serde::from_slice(&payload.0)
            .map_err(|error| PoolError::payload(PayloadKind::ProjectionResponse, error))?;
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Computing,
                cpu_nanos: cpu_nanos()?,
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

/// The worker's reader: every projection read of the run goes over its wire.
pub(crate) struct RemoteProjection {
    pub wire: Arc<Wire>,
}

impl RemoteProjection {
    #[expect(
        clippy::disallowed_methods,
        reason = "broken worker IPC terminates the worker and cannot become a catchable guest error"
    )]
    fn exchange(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> ProjectionAnswer {
        self.wire.read(resource, requests).unwrap_or_else(|error| {
            self.wire.refuse(&error);
            std::process::abort()
        })
    }
}

impl ProjectionReader for RemoteProjection {
    fn read(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionReadError> {
        Ok(self.exchange(resource, vec![request])?.pop().flatten())
    }

    fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionReadError> {
        self.exchange(resource, requests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_vm_client::ipc::read_frame;
    use lashlang::ProjectionType;

    /// ADR 0132 §9: a `read_range` over N requests of one resource costs one
    /// projection frame, not N.
    #[test]
    fn a_read_range_of_many_requests_is_one_projection_frame() {
        const REQUESTS: usize = 8;
        let config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, mut parent) = UnixStream::pair().expect("pipe");
        let fence = || MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
        let fences = Arc::new(Mutex::new(Fences {
            incoming: Some(fence()),
            outgoing: fence(),
            next_effect: 0,
        }));
        let reader = RemoteProjection {
            wire: Arc::new(Wire::new(
                pipe,
                Arc::default(),
                codec.clone(),
                fences,
                config.deadlines.serialization,
                config.tuning.parent_wait,
            )),
        };
        let resource = ResourceRef {
            projection: ProjectionType::new("rows"),
            id: "r".into(),
            revision: Some("1".into()),
        };
        let parent_codec = codec.clone();
        let parent = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut outgoing = fence();
            let mut reads = Vec::new();
            loop {
                let bytes = read_frame(&mut parent, &parent_codec, deadline).expect("frame");
                match parent_codec
                    .decode_worker(&bytes)
                    .expect("worker frame")
                    .message
                {
                    WorkerMessage::EffectRequest(request) => {
                        assert_eq!(request.kind, EffectKind::ProjectionRead);
                        let read: ProjectionRead =
                            rmp_serde::from_slice(&request.payload.0).expect("read");
                        let answer: ProjectionAnswer = Ok((0..read.requests.len())
                            .map(|index| Some(ProjectedReadResponse::Len(index)))
                            .collect());
                        reads.push(read);
                        let response = ParentFrame {
                            header: outgoing.next_header(),
                            message: ParentMessage::EffectResponse(EffectResponse {
                                id: request.id,
                                outcome: EffectOutcome::Value(EncodedPayload(
                                    rmp_serde::to_vec_named(&answer).expect("answer"),
                                )),
                            }),
                        };
                        let bytes = parent_codec.encode_parent(&response).expect("encode");
                        write_frame(&mut parent, &bytes, deadline).expect("write");
                    }
                    WorkerMessage::Progress {
                        phase: WorkerPhase::Computing,
                        ..
                    } => return reads,
                    _ => {}
                }
            }
        });

        let answers = reader
            .read_range(
                &resource,
                (0..REQUESTS).map(|_| ProjectedReadRequest::Len).collect(),
            )
            .expect("answers");
        let reads = parent.join().expect("parent");

        assert_eq!(reads.len(), 1, "one frame carries the whole batch");
        assert_eq!(reads[0].resource, resource);
        assert_eq!(reads[0].requests.len(), REQUESTS);
        assert_eq!(
            answers,
            (0..REQUESTS)
                .map(|index| Some(ProjectedReadResponse::Len(index)))
                .collect::<Vec<_>>(),
            "the batch is answered in order"
        );
    }
}
