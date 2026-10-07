//! The asynchronous broker's slots over the synchronous, owned process pool.

use crate::WorkerPoolRuntimeOps as _;
use lash_vm_broker::{CheckoutRefusal, WorkerCheckout, WorkerRead, WorkerSlots, WorkerTransport};
use lash_vm_protocol::*;
use tokio::sync::mpsc;

use crate::service::CallHeld;
use crate::{ExecutionBudget, ParkOutcome, WorkerPool};

/// One admitted run's pool access, with parent-owned accounting shared by all
/// its park/resume checkouts. A transport actor owns and reaps each checkout.
pub struct PoolSlots {
    pub pool: WorkerPool,
    pub owner_epoch: OwnerEpoch,
    pub frame_epoch: FrameEpoch,
    pub budget: ExecutionBudget,
    /// The service the run checks workers out through, which records each
    /// checkout when receipts are on.
    pub service: Option<crate::service::Service>,
}

#[async_trait::async_trait]
impl WorkerSlots for PoolSlots {
    async fn checkout(
        &self,
        _owner: &VmOwner,
        start: &Start,
    ) -> Result<WorkerCheckout, CheckoutRefusal> {
        let pool = self.pool.clone();
        let epoch = self.owner_epoch;
        let frame = self.frame_epoch;
        // A resumed continuation may be larger than the initial input.
        // Reserve this checkout's complete Start, before queue admission.
        let codec = FrameCodec::new(self.pool.config().protocol.decode);
        let mut fence = MessageFence::new(ExecutionLease(u64::MAX), epoch, frame);
        let reservation = codec
            .encode_parent(&ParentFrame {
                header: fence.next_header(),
                message: ParentMessage::Start(Box::new(start.clone())),
            })
            .map_err(|error| CheckoutRefusal::Infrastructure(error.into()))?
            .len()
            .saturating_add(128);
        let budget = self.budget.clone();
        let held = CallHeld::of(self.service.as_ref());
        let worker =
            tokio::task::spawn_blocking(move || pool.checkout(reservation, epoch, frame, budget))
                .await
                .map_err(|_| CheckoutRefusal::Closed)?
                .map_err(|error| match error {
                    crate::PoolError::QueueFull { .. } => CheckoutRefusal::QueueFull,
                    crate::PoolError::CheckoutTimedOut => CheckoutRefusal::TimedOut {
                        waited: self.pool.config().deadlines.checkout,
                    },
                    crate::PoolError::RestartStorm => CheckoutRefusal::RestartStorm,
                    error @ (crate::PoolError::Infrastructure(_)
                    | crate::PoolError::RetryLimitExceeded
                    | crate::PoolError::ProtocolVersion(_)
                    | crate::PoolError::InvalidConfiguration
                    | crate::PoolError::UnsupportedPlatform
                    | crate::PoolError::Io { .. }) => {
                        CheckoutRefusal::Infrastructure(error.into_outcome())
                    }
                })?;
        drop(held);
        #[cfg(feature = "testing")]
        if let Some(service) = &self.service {
            service
                .record_worker(
                    match &start.program {
                        ProgramSource::Artifact {
                            entry: ProgramEntry::Process { .. },
                            ..
                        } => crate::service::WorkerPath::Process,
                        _ => crate::service::WorkerPath::Cell,
                    },
                    &worker,
                )
                .map_err(|error| CheckoutRefusal::Infrastructure(error.into_outcome()))?;
        }
        let lease = worker.lease();
        let interruptor = worker.interruptor().map_err(|_| CheckoutRefusal::Closed)?;
        let codec = FrameCodec::new(self.pool.config().protocol.decode);
        // Each command travels with its call's hold, released once the
        // worker answered it.
        let (commands, mut inputs) = mpsc::channel::<(Option<Vec<u8>>, CallHeld)>(1);
        let (outputs, messages) = mpsc::channel(2);
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let actor_released = released.clone();
        let actor = tokio::task::spawn_blocking(move || {
            let mut worker = worker;
            let mut incoming = MessageFence::new(lease, epoch, frame);
            let mut outgoing = MessageFence::new(lease, epoch, frame);
            let send = |message, fence: &mut MessageFence| match codec.encode_worker(&WorkerFrame {
                header: fence.next_header(),
                message,
            }) {
                Ok(bytes) => {
                    let _ = outputs.blocking_send(WorkerRead::Bytes(bytes));
                }
                Err(_) => {
                    let _ =
                        outputs.blocking_send(WorkerRead::Ended(SupervisorEvidence::EndOfStream));
                }
            };
            // Checkout has already received and verified the real handshake.
            send(
                WorkerMessage::Ready {
                    protocol_version: WORKER_PROTOCOL_VERSION,
                    crate_version: env!("CARGO_PKG_VERSION").into(),
                },
                &mut outgoing,
            );
            while let Some((bytes, _held)) = inputs.blocking_recv() {
                let Some(bytes) = bytes else {
                    drop(worker);
                    return;
                };
                let admitted = codec
                    .decode_parent(&bytes)
                    .map_err(InfrastructureOutcome::from)
                    .and_then(|parent| match incoming.admit(&parent.header) {
                        Ok(()) => Ok(parent.message),
                        Err(refusal) => Err(ProtocolBreach::from(refusal).into()),
                    });
                let parent = match admitted {
                    Ok(parent) => parent,
                    Err(outcome) => {
                        let _ = outputs.blocking_send(WorkerRead::Failed(outcome));
                        return;
                    }
                };
                let response = match parent {
                    ParentMessage::Start(start) => worker.start(*start),
                    ParentMessage::EffectResponse(result) => worker.effect_result(result),
                    ParentMessage::Park => worker.park().map(|parked| match parked {
                        ParkOutcome::Parked(state) => WorkerMessage::Suspended { state },
                        ParkOutcome::Declined(request) => WorkerMessage::EffectRequest(request),
                    }),
                    ParentMessage::Cancel => worker.cancel(),
                    _ => break,
                };
                let observations = worker.take_observations();
                if matches!(
                    &response,
                    Ok(WorkerMessage::Complete { .. }
                        | WorkerMessage::GuestError { .. }
                        | WorkerMessage::Suspended { .. }
                        | WorkerMessage::Cancelled)
                ) {
                    let released_worker = if matches!(
                        &response,
                        Ok(WorkerMessage::GuestError { .. } | WorkerMessage::Cancelled)
                    ) {
                        drop(worker);
                        Ok(())
                    } else {
                        worker.release()
                    };
                    let result = released_worker;
                    actor_released.store(true, std::sync::atomic::Ordering::Release);
                    if let Err(error) = result {
                        let _ = outputs.blocking_send(WorkerRead::Failed(error.into_outcome()));
                        return;
                    }
                    for payload in observations {
                        send(WorkerMessage::Observations { payload }, &mut outgoing);
                    }
                    if let Ok(response) = response {
                        send(response, &mut outgoing);
                    }
                    return;
                }
                for payload in observations {
                    send(WorkerMessage::Observations { payload }, &mut outgoing);
                }
                match response.map_err(crate::PoolError::into_outcome) {
                    Ok(response) => send(response, &mut outgoing),
                    Err(outcome) => {
                        match outcome {
                            InfrastructureOutcome::WorkerLimitExceeded { limit } => {
                                send(WorkerMessage::LimitExceeded { limit }, &mut outgoing)
                            }
                            InfrastructureOutcome::WorkerCrashed { evidence } => {
                                let _ = outputs.blocking_send(WorkerRead::Ended(evidence));
                            }
                            InfrastructureOutcome::WorkerUnresponsive { silent_ms } => {
                                let _ =
                                    outputs.blocking_send(WorkerRead::Unresponsive { silent_ms });
                            }
                            outcome @ (InfrastructureOutcome::WorkerDeployment { .. }
                            | InfrastructureOutcome::ProtocolViolation { .. }
                            | InfrastructureOutcome::RunRefused { .. }) => {
                                let _ = outputs.blocking_send(WorkerRead::Failed(outcome));
                            }
                        }
                        return;
                    }
                }
            }
            drop(worker);
            let _ = outputs.blocking_send(WorkerRead::Ended(SupervisorEvidence::EndOfStream));
        });
        Ok(WorkerCheckout {
            lease,
            transport: Box::new(Transport {
                service: self.service.clone(),
                commands,
                messages,
                actor: Some(actor),
                interruptor,
                released,
            }),
        })
    }
    async fn release(&self, mut checkout: WorkerCheckout) -> Result<(), CheckoutRefusal> {
        checkout
            .transport
            .send(Vec::new())
            .await
            .map_err(|_| CheckoutRefusal::Closed)?;
        Ok(())
    }
    async fn discard(&self, mut checkout: WorkerCheckout) -> Result<(), CheckoutRefusal> {
        checkout.transport.kill().await;
        Ok(())
    }
}
struct Transport {
    /// The service whose call hold each command carries.
    service: Option<crate::service::Service>,
    commands: mpsc::Sender<(Option<Vec<u8>>, CallHeld)>,
    messages: mpsc::Receiver<WorkerRead>,
    actor: Option<tokio::task::JoinHandle<()>>,
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    interruptor: std::os::unix::net::UnixStream,
}
#[async_trait::async_trait]
impl WorkerTransport for Transport {
    async fn send(&mut self, bytes: Vec<u8>) -> Result<(), SupervisorEvidence> {
        let reset = bytes.is_empty();
        if reset && self.released.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(actor) = self.actor.take() {
                actor.await.map_err(|_| SupervisorEvidence::EndOfStream)?;
            }
            return Ok(());
        }
        self.commands
            .send((
                (!reset).then_some(bytes),
                CallHeld::of(self.service.as_ref()),
            ))
            .await
            .map_err(|_| SupervisorEvidence::EndOfStream)?;
        if reset && let Some(actor) = self.actor.take() {
            actor.await.map_err(|_| SupervisorEvidence::EndOfStream)?;
        }
        Ok(())
    }
    async fn recv(&mut self) -> WorkerRead {
        self.messages
            .recv()
            .await
            .unwrap_or(WorkerRead::Ended(SupervisorEvidence::EndOfStream))
    }
    async fn kill(&mut self) -> SupervisorEvidence {
        self.messages.close();
        let _ = self.interruptor.shutdown(std::net::Shutdown::Both);
        let _ = self.commands.try_send((None, CallHeld::of(None)));
        if let Some(actor) = self.actor.take() {
            let _ = actor.await;
        }
        SupervisorEvidence::EndOfStream
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if self.actor.is_some() {
            let _ = self.interruptor.shutdown(std::net::Shutdown::Both);
        }
    }
}
