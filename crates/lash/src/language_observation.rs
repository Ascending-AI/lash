//! Language facts enter bounded replay outside VM execution. Each class has
//! its own count/byte budget and worker; process facts keep one FIFO barrier.
//! A process fact too large to admit loses its own process's continuity, in
//! its place in that FIFO; a queue that overflows loses every process's.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::facade_support::ObservedProcessEvent;
use lash_core::{
    LiveReplayEventDraft, LiveReplayStore, ProcessReplayEventDraft, ProcessReplayStore,
    ProcessSequence, SessionObservationEventPayload,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{ProcessId, SessionId};
use lash_trace::{
    LanguageExecutionObservation, TraceEvent, TraceRecord, TraceSink, TraceSinkError,
};

mod ingress;
mod routing;
mod worker;
use ingress::Ingress;
use routing::Subject;
pub(crate) use worker::CommittedPublication;
use worker::{Channel, ProcessPublication, ProcessPublicationMarks};

pub(crate) struct LanguageObservationPublisher {
    process: Arc<Channel<ProcessPublication>>,
    session: Arc<Channel<(SessionId, LiveReplayEventDraft)>>,
    process_store: Arc<dyn ProcessReplayStore>,
    session_store: Arc<dyn LiveReplayStore>,
    /// The committed facts this node is still publishing, per process.
    committing: Arc<ProcessPublicationMarks>,
    workers: Mutex<Option<[tokio::task::AbortHandle; 2]>>,
    runtime: Option<tokio::runtime::Handle>,
    closed: AtomicBool,
    /// The most one draft may charge its ingress.
    max_bytes: usize,
}

impl LanguageObservationPublisher {
    pub(crate) fn new(
        process_store: Arc<dyn ProcessReplayStore>,
        session_store: Arc<dyn LiveReplayStore>,
    ) -> Self {
        Self::with_limits(
            process_store,
            session_store,
            ingress::MAX_EVENTS,
            ingress::MAX_BYTES,
        )
    }

    fn with_limits(
        process_store: Arc<dyn ProcessReplayStore>,
        session_store: Arc<dyn LiveReplayStore>,
        events: usize,
        bytes: usize,
    ) -> Self {
        Self {
            process: Arc::new(Channel::new(Ingress::new(events, bytes))),
            session: Arc::new(Channel::new(Ingress::new(events, bytes))),
            process_store,
            session_store,
            committing: Arc::default(),
            workers: Mutex::new(None),
            runtime: tokio::runtime::Handle::try_current().ok(),
            closed: AtomicBool::new(false),
            max_bytes: bytes,
        }
    }

    fn charge(&self, value: &impl serde::Serialize) -> Option<usize> {
        worker::charge(value, self.max_bytes)
    }

    /// Admit one draft of `process` behind those already accepted. A `None`
    /// charge is a fact the ingress cannot take: the worker invalidates that
    /// process in the fact's place. A commit's own publication names the
    /// sequence it stands on in `committed_over`, and once admitted holds
    /// its process's publication window open until the worker is done with
    /// it or it was dropped.
    fn enqueue_process(
        &self,
        process: &ProcessId,
        charge: Option<usize>,
        committed_over: Option<ProcessSequence>,
        draft: impl FnOnce() -> ProcessReplayEventDraft,
    ) {
        self.process
            .enqueue(charge.or(Some(worker::LOST_CHARGE)), || {
                ProcessPublication {
                    id: process.clone(),
                    draft: charge.map(|_| draft()),
                    completion: None,
                    mark: committed_over.map(|base| self.committing.hold(process, base)),
                }
            });
        self.start();
    }

    fn start(&self) {
        let mut workers = self.workers.lock_recover();
        if self.closed.load(Ordering::Acquire) || workers.is_some() {
            return;
        }
        let Some(runtime) = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        *workers = Some([
            runtime
                .spawn(worker::process(
                    Arc::clone(&self.process),
                    Arc::clone(&self.process_store),
                ))
                .abort_handle(),
            runtime
                .spawn(worker::session(
                    Arc::clone(&self.session),
                    Arc::clone(&self.session_store),
                ))
                .abort_handle(),
        ]);
    }

    /// Called after the durable commit. Admission never waits on the store;
    /// the FIFO drains accepted preceding language observations first.
    pub(crate) fn enqueue_committed(&self, process: &ProcessId, event: ObservedProcessEvent) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let charge = self.charge(&(process, &event));
        let base = ProcessSequence::new(event.sequence.saturating_sub(1));
        self.enqueue_process(process, charge, Some(base), || {
            ProcessReplayEventDraft::committed(event)
        });
    }

    /// The publication window of `process`'s durable sequence `durable`,
    /// read just now: whether this node is still publishing a fact at or
    /// before it.
    pub(crate) fn publication_window(
        &self,
        process: &ProcessId,
        durable: ProcessSequence,
    ) -> lash_core::runtime::durable::services::PublicationWindow<ProcessId, ProcessSequence> {
        self.committing.window(process, durable)
    }

    /// Recovery waits for the same FIFO to publish the fact before checking
    /// its bridge. This acknowledgement is never awaited by execution.
    ///
    /// Every fact the dispatcher drops or the store refuses is answered
    /// [`CommittedPublication::ContinuityLost`]. A fact too large to admit
    /// is answered so without invalidating anything: no publication was
    /// lost, and the feed that asked rebuilds from the durable process.
    pub(crate) async fn publish_committed(
        &self,
        process: &ProcessId,
        event: ObservedProcessEvent,
    ) -> CommittedPublication {
        if self.closed.load(Ordering::Acquire) {
            return CommittedPublication::Stopped;
        }
        let Some(charge) = self.charge(&(process, &event)) else {
            return CommittedPublication::ContinuityLost;
        };
        let (completion, published) = tokio::sync::oneshot::channel();
        let admitted = self.process.enqueue(Some(charge), || ProcessPublication {
            id: process.clone(),
            draft: Some(ProcessReplayEventDraft::committed(event)),
            completion: Some(completion),
            mark: None,
        });
        self.start();
        if !admitted {
            return CommittedPublication::ContinuityLost;
        }
        if self.closed.load(Ordering::Acquire) {
            return CommittedPublication::Stopped;
        }
        // Only a stopped worker drops an acknowledgement unanswered.
        published.await.unwrap_or(CommittedPublication::Stopped)
    }

    /// Stop after execution has stopped; any undrained draft is explicit loss.
    pub(crate) async fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(workers) = self.workers.lock_recover().take() {
            for worker in workers {
                worker.abort();
            }
        }
        if self.process.has_work()
            && let Err(error) = self.process_store.invalidate_all().await
        {
            tracing::warn!(%error, "process language replay shutdown lost continuity");
        }
        if self.session.has_work()
            && let Err(error) = self.session_store.invalidate_all().await
        {
            tracing::warn!(%error, "session language replay shutdown lost continuity");
        }
        self.process.stop();
    }

    fn enqueue_language(
        &self,
        record: &TraceRecord,
        language: &str,
        execution: &lash_trace::TraceLanguageExecution,
    ) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let Some(subject) = routing::subject(execution) else {
            return;
        };
        let timestamp = u64::try_from(record.timestamp.timestamp_millis()).ok();
        let charge = timestamp.and_then(|_| match &subject {
            Subject::Process(id) => self.charge(&(id, language, execution)),
            Subject::Session(id) => self.charge(&(id, language, execution)),
        });
        let observation = || LanguageExecutionObservation {
            language: language.into(),
            execution: execution.clone(),
            observed_at_ms: timestamp.unwrap_or(0),
        };
        match subject {
            Subject::Process(process) => self.enqueue_process(&process, charge, None, || {
                ProcessReplayEventDraft::language_execution(ProcessSequence(0), observation())
            }),
            Subject::Session(session) => {
                self.session.enqueue(charge, || {
                    (
                        session,
                        LiveReplayEventDraft::new(
                            execution.identity.scope.turn_id.clone(),
                            SessionObservationEventPayload::LanguageExecution(observation()),
                        ),
                    )
                });
                self.start();
            }
        }
    }
}

impl LanguageObservationPublisher {
    /// A step's body start joins its process's FIFO behind the language
    /// observations already accepted for it.
    fn enqueue_step_body_started(&self, record: &TraceRecord, step: &lash_trace::StepBodyStarted) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let timestamp = u64::try_from(record.timestamp.timestamp_millis()).ok();
        let charge = timestamp.and_then(|_| self.charge(step));
        self.enqueue_process(&step.process_id, charge, None, || {
            ProcessReplayEventDraft::step_body_started(
                ProcessSequence(0),
                lash_trace::StepBodyStartedObservation {
                    step: step.clone(),
                    observed_at_ms: timestamp.unwrap_or(0),
                },
            )
        });
    }
}

impl TraceSink for LanguageObservationPublisher {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        match &record.event {
            TraceEvent::LanguageExecution { language, event } => {
                self.enqueue_language(record, language, event);
            }
            TraceEvent::StepBodyStarted { step } => self.enqueue_step_body_started(record, step),
            _ => {}
        }
        Ok(())
    }
}

impl Drop for LanguageObservationPublisher {
    fn drop(&mut self) {
        if let Some(workers) = self.workers.lock_recover().take() {
            for worker in workers {
                worker.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests;
