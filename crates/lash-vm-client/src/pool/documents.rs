//! Checkout-owned canonical sources for process-local document cache misses.
use super::*;
use lash_kernel_value::DocumentId;
use lash_vm_protocol::message::StoredDocument;

impl Checkout {
    /// Takes the run for the worker: with the run's first slice, it
    /// validates and compiles `start`'s document and builds a machine that
    /// has executed nothing, or one rebuilt from the parked state; what it
    /// refuses, that slice answers (FIG-5921). `document` is the identity
    /// the parent knows the document by, which a parked state must name;
    /// `class` is what the pool's receipts call the execution. A lease
    /// whose `prepare` lowered the run's document starts it with no reset
    /// between. A start asking for more of a bound than the pool's
    /// [`PoolConfig::run_bounds`] admit is refused with
    /// [`RunRefusal::AboveCeiling`] before anything crosses.
    pub fn start(
        &mut self,
        mut start: Start,
        document: &str,
        class: ExecutionClass,
    ) -> Result<(), PoolError> {
        if !matches!(
            self.state.live()?.phase,
            LeasePhase::Fresh | LeasePhase::Prepared { .. }
        ) {
            return Err(PoolError::breach(SequenceFault::CheckoutAlreadyStarted));
        }
        // The host configuration is the ceiling the pool admits, never a
        // clamp: a run asking more is refused, naming both (FIG-6827).
        if let Some(refusal) = start.bounds.past(&self.pool.config.run_bounds) {
            return Err(PoolError::refused(refusal));
        }
        self.pool.library.prepare_start(&mut start)?;
        let document_sources = Self::reference_documents(&mut start)?;
        // The start crosses within what the checkout reserved for it, as
        // if it crossed alone.
        let worker = &self.state.live()?.worker;
        let size = worker
            .codec
            .borrowed_frame_len(&BorrowedParentFrame {
                header: self.outgoing.next_header_copy(),
                message: BorrowedParentMessage::Start(&start),
            })
            .map_err(PoolError::from);
        match size {
            Ok(size) if size <= self.reservation as u64 => {}
            Ok(size) => {
                self.discard();
                return Err(PoolError::QueueFull {
                    bytes: usize::try_from(size).unwrap_or(usize::MAX),
                });
            }
            Err(error) => {
                self.discard();
                return Err(error);
            }
        }
        let run = Run {
            owner: start.owner.clone(),
            document: document.to_owned(),
            class,
            computed: false,
            print_budget: start.bounds.memory,
            state: RunState::Ready(ReadyRun {
                start: StartDispatch::Queued(Box::new(start)),
                deliveries: Vec::new(),
            }),
            document_sources,
            grants: Vec::new(),
        };
        self.state.live_mut()?.phase = LeasePhase::Run(Box::new(run));
        self.phase_identity();
        if let Some(scope) = self.phase_scope() {
            scope.point("document_start");
        }
        Ok(())
    }

    fn reference_documents(
        start: &mut Start,
    ) -> Result<std::collections::BTreeMap<DocumentId, EncodedPayload>, PoolError> {
        let mut sources = std::collections::BTreeMap::new();
        for supplied in std::iter::once(&mut start.document).chain(&mut start.documents) {
            if let StoredDocument::Supplied { document, body } = supplied {
                let id = *document;
                let body = EncodedPayload(std::mem::take(&mut body.0));
                if sources.get(&id).is_some_and(|known| known != &body) {
                    return Err(PoolError::payload(
                        lash_protocol_types::engine_fault::PayloadKind::Start,
                        "conflicting document bodies",
                    ));
                }
                sources.insert(id, body);
                *supplied = StoredDocument::Cached { document: id };
            }
        }
        Ok(sources)
    }

    pub(super) fn answer_documents(
        &mut self,
        requested: Vec<DocumentId>,
        timeout: Duration,
    ) -> Result<(), PoolError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut documents = Vec::with_capacity(requested.len());
        for document in requested {
            if !seen.insert(document) {
                return Err(PoolError::payload(
                    lash_protocol_types::engine_fault::PayloadKind::Start,
                    "duplicate document miss",
                ));
            }
            let body = self
                .state
                .run()?
                .document_sources
                .get(&document)
                .ok_or_else(|| {
                    PoolError::refused(
                        lash_protocol_types::engine_fault::RunRefusal::DocumentNotSupplied {
                            document,
                        },
                    )
                })?;
            documents.push(StoredDocument::Supplied {
                document,
                body: body.clone(),
            });
        }
        self.send(ParentMessage::Documents { documents }, timeout)
    }
}
