//! Process-local verified canonical Documents. Resets retain pure code;
//! process death drops it. A miss always asks the declaring parent.
use crate::PoolError;
use lash_kernel_doc::IdentifiedDocument;
use lash_vocabulary::{DocumentId, KernelDataLimits};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

struct Entry {
    document: IdentifiedDocument,
    admitted: OnceLock<(KernelDataLimits, Result<IdentifiedDocument, String>)>,
}
#[derive(Default)]
pub(crate) struct Documents(Mutex<VecDeque<Arc<Entry>>>);
impl Documents {
    pub(crate) fn keep(&self, document: IdentifiedDocument, limits: Option<KernelDataLimits>) {
        const BYTES: usize = 8 * 1024 * 1024;
        if document.body().len() > BYTES {
            return;
        }
        let admitted = limits
            .map(|limits| (limits, Ok(document.clone())))
            .map(OnceLock::from)
            .unwrap_or_default();
        let candidate = Arc::new(Entry { document, admitted });
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = held
            .iter()
            .position(|known| known.document.id() == candidate.document.id())
        {
            if let Some(known) = held.remove(at) {
                held.push_back(known);
            }
            return;
        }
        while held.len() >= 16
            || held
                .iter()
                .map(|entry| entry.document.body().len())
                .sum::<usize>()
                + candidate.document.body().len()
                > BYTES
        {
            held.pop_front();
        }
        held.push_back(candidate);
    }
    pub(crate) fn get(
        &self,
        id: DocumentId,
        limits: KernelDataLimits,
    ) -> Result<Option<IdentifiedDocument>, PoolError> {
        let known = {
            let mut held = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let found = held
                .iter()
                .position(|entry| entry.document.id() == id)
                .and_then(|at| held.remove(at));
            if let Some(entry) = &found {
                held.push_back(Arc::clone(entry));
            }
            found
        };
        let Some(entry) = known else {
            return Ok(None);
        };
        let decode = || {
            IdentifiedDocument::decode(entry.document.body(), limits)
                .map_err(|error| error.to_string())
        };
        let (checked_limits, checked) = entry.admitted.get_or_init(|| (limits, decode()));
        let decoded = if *checked_limits == limits {
            checked.clone()
        } else {
            decode()
        };
        decoded.map(Some).map_err(|error| {
            PoolError::refused(lash_protocol_types::engine_fault::RunRefusal::Undecodable {
                input: lash_protocol_types::engine_fault::RunInput::Document,
                detail: lash_protocol_types::engine_fault::Detail::new(error),
            })
        })
    }
}
