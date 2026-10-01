//! The asynchronous broker's slots over the synchronous, owned process pool.

use lash_vm_broker::{CheckoutRefusal, WorkerCheckout, WorkerRead, WorkerSlots, WorkerTransport};
use lash_vm_protocol::*;
use tokio::sync::mpsc;

use crate::{ExecutionBudget, ParkOutcome, WorkerPool};

/// One admitted run's pool access, with parent-owned accounting shared by all
/// its park/resume checkouts. A transport actor owns and reaps each checkout.
pub struct PoolSlots {
    pub pool: WorkerPool,
    pub owner_epoch: OwnerEpoch,
    pub frame_epoch: FrameEpoch,
    pub budget: ExecutionBudget,
    pub recovery: Option<crate::service::Service>,
}

#[async_trait::async_trait]
impl WorkerSlots for PoolSlots {
    async fn checkout(
        &self,
        _owner: &VmOwner,
        start: &Start,
    ) -> Result<WorkerCheckout, CheckoutRefusal> {
        if let Some(service) = &self.recovery {
            service.mark_running().await.map_err(recovery_refusal)?;
        }
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
                    crate::PoolError::Infrastructure(outcome) => {
                        CheckoutRefusal::Infrastructure(outcome)
                    }
                    crate::PoolError::RetryLimitExceeded => CheckoutRefusal::Infrastructure(
                        InfrastructureOutcome::WorkerLimitExceeded {
                            limit: WorkerLimit::Deadline,
                        },
                    ),
                    _ => CheckoutRefusal::Closed,
                })?;
        #[cfg(feature = "testing")]
        if let Some(service) = &self.recovery {
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
                .map_err(recovery_refusal)?;
        }
        let lease = worker.lease();
        let interruptor = worker.interruptor().map_err(|_| CheckoutRefusal::Closed)?;
        let codec = FrameCodec::new(self.pool.config().protocol.decode);
        let (commands, mut inputs) = mpsc::channel::<Option<Vec<u8>>>(1);
        let (outputs, messages) = mpsc::channel(2);
        let recovery = self.recovery.clone();
        let runtime = tokio::runtime::Handle::current();
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
            while let Some(bytes) = inputs.blocking_recv() {
                let Some(bytes) = bytes else {
                    drop(worker);
                    return;
                };
                let parent = match codec.decode_parent(&bytes) {
                    Ok(parent) if incoming.admit(&parent.header).is_ok() => parent.message,
                    _ => {
                        let _ = outputs.blocking_send(WorkerRead::Failed(
                            InfrastructureOutcome::ProtocolViolation {
                                reason: "pool actor refused the parent frame or fence".into(),
                            },
                        ));
                        return;
                    }
                };
                if let Some(service) = &recovery
                    && let Err(error) = runtime.block_on(service.mark_running())
                {
                    let _ = outputs.blocking_send(WorkerRead::Failed(
                        InfrastructureOutcome::ProtocolViolation {
                            reason: error.to_string(),
                        },
                    ));
                    return;
                }
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
                if matches!(&response, Ok(WorkerMessage::EffectRequest(_)))
                    && let Some(service) = &recovery
                    && let Err(error) = runtime.block_on(service.checkpoint())
                {
                    let _ = outputs.blocking_send(WorkerRead::Failed(
                        InfrastructureOutcome::ProtocolViolation {
                            reason: error.to_string(),
                        },
                    ));
                    return;
                }
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
                    let result = released_worker.and_then(|()| match &recovery {
                        Some(service) => runtime.block_on(service.checkpoint()),
                        None => Ok(()),
                    });
                    actor_released.store(true, std::sync::atomic::Ordering::Release);
                    if let Err(error) = result {
                        let _ = outputs.blocking_send(WorkerRead::Failed(
                            InfrastructureOutcome::ProtocolViolation {
                                reason: error.to_string(),
                            },
                        ));
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
                match response {
                    Ok(response) => send(response, &mut outgoing),
                    Err(crate::PoolError::Infrastructure(outcome)) => {
                        match outcome {
                            InfrastructureOutcome::WorkerLimitExceeded { limit } => {
                                send(WorkerMessage::LimitExceeded { limit }, &mut outgoing)
                            }
                            outcome @ InfrastructureOutcome::PayloadTooLarge { .. } => {
                                let _ = outputs.blocking_send(WorkerRead::Failed(outcome));
                            }
                            InfrastructureOutcome::WorkerCrashed { evidence } => {
                                let _ = outputs.blocking_send(WorkerRead::Ended(evidence));
                            }
                            InfrastructureOutcome::WorkerUnresponsive { silent_ms } => {
                                let _ =
                                    outputs.blocking_send(WorkerRead::Unresponsive { silent_ms });
                            }
                            outcome @ InfrastructureOutcome::ProtocolViolation { .. } => {
                                let _ = outputs.blocking_send(WorkerRead::Failed(outcome));
                            }
                        }
                        return;
                    }
                    Err(error) => {
                        let _ = outputs.blocking_send(WorkerRead::Failed(
                            InfrastructureOutcome::ProtocolViolation {
                                reason: error.to_string(),
                            },
                        ));
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
        if let Some(service) = &self.recovery {
            service.checkpoint().await.map_err(recovery_refusal)?;
        }
        Ok(())
    }
    async fn discard(&self, mut checkout: WorkerCheckout) -> Result<(), CheckoutRefusal> {
        checkout.transport.kill().await;
        if let Some(service) = &self.recovery {
            service.checkpoint().await.map_err(recovery_refusal)?;
        }
        Ok(())
    }
}
struct Transport {
    commands: mpsc::Sender<Option<Vec<u8>>>,
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
            .send(if reset { None } else { Some(bytes) })
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
        let _ = self.commands.try_send(None);
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

fn recovery_refusal(error: crate::PoolError) -> CheckoutRefusal {
    match error {
        crate::PoolError::Infrastructure(outcome) => CheckoutRefusal::Infrastructure(outcome),
        error => CheckoutRefusal::Infrastructure(InfrastructureOutcome::ProtocolViolation {
            reason: error.to_string(),
        }),
    }
}
