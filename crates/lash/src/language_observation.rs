//! Language facts enter bounded replay outside VM execution. Each class has
//! its own count/byte budget and worker; process facts keep one FIFO barrier.

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
use worker::{Channel, ProcessPublication};

pub(crate) struct LanguageObservationPublisher {
    process: Arc<Channel<ProcessPublication>>,
    session: Arc<Channel<(SessionId, LiveReplayEventDraft)>>,
    process_store: Arc<dyn ProcessReplayStore>,
    session_store: Arc<dyn LiveReplayStore>,
    workers: Mutex<Option<[tokio::task::AbortHandle; 2]>>,
    runtime: Option<tokio::runtime::Handle>,
    closed: AtomicBool,
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
            workers: Mutex::new(None),
            runtime: tokio::runtime::Handle::try_current().ok(),
            closed: AtomicBool::new(false),
        }
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
        let charge = worker::charge(&(process, &event));
        self.process.enqueue(charge, || ProcessPublication {
            id: process.clone(),
            draft: ProcessReplayEventDraft::committed(event),
            completion: None,
        });
        self.start();
    }

    /// Recovery waits for the same FIFO to publish the fact before checking
    /// its bridge. This acknowledgement is never awaited by execution.
    pub(crate) async fn publish_committed(
        &self,
        process: &ProcessId,
        event: ObservedProcessEvent,
    ) -> Result<(), lash_core::ProcessReplayStoreError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(lash_core::ProcessReplayStoreError::Closed);
        }
        let (completion, published) = tokio::sync::oneshot::channel();
        let charge = worker::charge(&(process, &event));
        self.process.enqueue(charge, || ProcessPublication {
            id: process.clone(),
            draft: ProcessReplayEventDraft::committed(event),
            completion: Some(completion),
        });
        self.start();
        published
            .await
            .unwrap_or(Err(lash_core::ProcessReplayStoreError::Closed))
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
            Subject::Process(id) => worker::charge(&(id, language, execution)),
            Subject::Session(id) => worker::charge(&(id, language, execution)),
        });
        let observation = || LanguageExecutionObservation {
            language: language.into(),
            execution: execution.clone(),
            observed_at_ms: timestamp.unwrap_or(0),
        };
        match subject {
            Subject::Process(process) => self.process.enqueue(charge, || ProcessPublication {
                id: process,
                draft: ProcessReplayEventDraft::language_execution(
                    ProcessSequence(0),
                    observation(),
                ),
                completion: None,
            }),
            Subject::Session(session) => self.session.enqueue(charge, || {
                (
                    session,
                    LiveReplayEventDraft::new(
                        execution.identity.scope.turn_id.clone(),
                        SessionObservationEventPayload::LanguageExecution(observation()),
                    ),
                )
            }),
        }
        self.start();
    }
}

impl TraceSink for LanguageObservationPublisher {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        if let TraceEvent::LanguageExecution { language, event } = &record.event {
            self.enqueue_language(record, language, event);
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
