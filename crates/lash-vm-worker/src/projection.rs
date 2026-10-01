//! Pure, demand-driven projection reads under the run's real IPC fence.
use crate::worker::{Fences, cpu_nanos};
use lash_vm_client::{
    PoolError, ProjectionRead,
    ipc::{FrameSource, write_frame},
};
use lash_vm_protocol::*;
use lashlang::{
    ProjectedHostDescriptor, ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record,
    Value,
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
    namespace: String,
}
impl Wire {
    pub fn new(
        pipe: UnixStream,
        inbound: Arc<Mutex<FrameSource>>,
        codec: FrameCodec,
        fences: Arc<Mutex<Fences>>,
        namespace: String,
    ) -> Self {
        Self {
            pipe: Mutex::new(pipe),
            inbound,
            codec,
            fences,
            namespace,
        }
    }
    fn send(&self, pipe: &mut UnixStream, message: WorkerMessage) -> Result<(), PoolError> {
        let bytes = self
            .fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .encode(&self.codec, message)?;
        write_frame(pipe, &bytes, Instant::now() + Duration::from_secs(30))
    }
    fn read(
        self: &Arc<Self>,
        key: usize,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, PoolError> {
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
                    rmp_serde::to_vec_named(&ProjectionRead { key, request })
                        .map_err(PoolError::protocol)?,
                ),
            }),
        )?;
        let bytes = self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .read_frame(
                &mut pipe,
                &self.codec,
                Instant::now() + Duration::from_secs(86_400),
            )?;
        let frame = self.codec.decode_parent(&bytes)?;
        self.fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .incoming
            .as_mut()
            .ok_or_else(|| PoolError::protocol("projection has no run fence"))?
            .admit(&frame.header)
            .map_err(PoolError::protocol)?;
        let ParentMessage::EffectResponse(EffectResponse {
            id: response_id,
            outcome: EffectOutcome::Value(payload),
        }) = frame.message
        else {
            return Err(PoolError::protocol("projection received another control"));
        };
        if response_id != id {
            return Err(PoolError::protocol("projection response has another ID"));
        }
        self.codec.check_payload(&payload.0)?;
        let response: Option<ProjectedReadResponse> =
            rmp_serde::from_slice(&payload.0).map_err(PoolError::protocol)?;
        self.send(
            &mut pipe,
            WorkerMessage::Progress {
                phase: WorkerPhase::Computing,
                cpu_nanos: cpu_nanos()?,
            },
        )?;
        Ok(response.map(|response| match response {
            ProjectedReadResponse::Value(value) => ProjectedReadResponse::Value(self.rebind(value)),
            other => other,
        }))
    }
    pub fn refuse(&self, error: &PoolError) {
        let mut pipe = self
            .pipe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = self.send(
            &mut pipe,
            match error {
                PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded { limit }) => {
                    WorkerMessage::LimitExceeded { limit: *limit }
                }
                PoolError::Infrastructure(outcome) => WorkerMessage::Refused {
                    outcome: outcome.clone(),
                },
                error => WorkerMessage::Refused {
                    outcome: InfrastructureOutcome::ProtocolViolation {
                        reason: error.to_string(),
                    },
                },
            },
        );
    }
    pub fn resolve(self: &Arc<Self>, value: &ProjectedValue) -> Option<ProjectedValue> {
        let rest = value.name().strip_prefix("worker-projection/")?;
        let (namespace, rest) = rest.split_once('/')?;
        if namespace != self.namespace {
            return None;
        }
        let (key, _) = rest.split_once('/')?;
        Some(ProjectedValue::custom(
            value.name().to_owned(),
            Arc::new(RemoteProjection {
                wire: self.clone(),
                key: key.parse().ok()?,
                type_name: value.type_name().to_owned(),
            }),
        ))
    }
    pub fn rebind_outcome(
        self: &Arc<Self>,
        outcome: lashlang::AbilityOutcome,
    ) -> lashlang::AbilityOutcome {
        use lashlang::{
            AbilityOutcome, ResourceOperationBatchOutcome as Batch,
            ResourceOperationOutcome as Leaf,
        };
        let leaf = |leaf| match leaf {
            Leaf::Value(value) => Leaf::Value(self.rebind(value)),
            other => other,
        };
        match outcome {
            AbilityOutcome::Value(value) => AbilityOutcome::Value(self.rebind(value)),
            AbilityOutcome::ResourceOperationBatch(Batch::AllResults(values)) => {
                AbilityOutcome::ResourceOperationBatch(Batch::AllResults(
                    values.into_iter().map(leaf).collect(),
                ))
            }
            AbilityOutcome::ResourceOperationBatch(Batch::Selected {
                leaf: index,
                result,
            }) => AbilityOutcome::ResourceOperationBatch(Batch::Selected {
                leaf: index,
                result: leaf(result),
            }),
            other => other,
        }
    }
    pub fn rebind(self: &Arc<Self>, value: Value) -> Value {
        match value {
            Value::Projected(value) => Value::Projected(self.resolve(&value).unwrap_or(value)),
            Value::List(values) => Value::List(
                values
                    .iter()
                    .cloned()
                    .map(|v| self.rebind(v))
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Value::Tuple(values) => Value::Tuple(
                values
                    .iter()
                    .cloned()
                    .map(|v| self.rebind(v))
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Value::Record(values) => Value::Record(Arc::new(
                values
                    .iter()
                    .map(|(k, v)| (k.to_string(), self.rebind(v.clone())))
                    .collect::<Record>(),
            )),
            other => other,
        }
    }
}
pub(crate) struct RemoteProjection {
    pub wire: Arc<Wire>,
    pub key: usize,
    pub type_name: String,
}
impl ProjectedHostDescriptor for RemoteProjection {
    fn type_name(&self) -> &str {
        &self.type_name
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "broken worker IPC terminates the worker and cannot become a catchable guest error"
    )]
    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        self.wire.read(self.key, request).unwrap_or_else(|error| {
            self.wire.refuse(&error);
            std::process::abort()
        })
    }
}
