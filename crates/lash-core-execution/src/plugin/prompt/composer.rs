//! The prompt composer (FIG-5256, ADR 0133 §5 to §7): the one route from a
//! session's registered sections to a call's instruction text and its
//! snapshot.
//!
//! [`PromptCatalog::compose`] resolves the host's plan, renders every
//! section's base text and then its wrapper chain on a bounded
//! [`PromptRenderPool`], and assembles the result in plan order, never in
//! completion order. It fails closed: a refusal, panic, oversize text or a
//! render that outlasts the plan's budget composes nothing, and no earlier
//! text stands in. A late render's result is dropped.
//!
//! A [`ComposedPrompt`] records as one snapshot root whose texts are stored
//! by content address, so a section that did not change between calls is
//! stored once. [`load_prompt_snapshot`] reads an admitted call's snapshot
//! back, every text verified against its address, without calling any
//! renderer or wrapper.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};

use lash_durable::domain::{PromptCallKey, PromptText, PromptWrite};
use lash_durable::{DomainWrite, DurableError, DurableReads};
use lash_sansio::sync::MutexExt;

use super::{
    PromptCatalog, PromptCompositionError, PromptCut, PromptPlacement, PromptPlan, PromptPurpose,
    PromptSnapshot, PromptSnapshotVersion, PromptTextRef, RecordedSectionText,
    ResolvedPromptComposition,
};
use crate::store::BlobRef;

/// What separates two sections' text within one placement.
pub const PROMPT_SECTION_SEPARATOR: &str = "\n\n";

type RenderJob = Box<dyn FnOnce() + Send>;

/// A bounded executor for section renderers: a fixed set of worker threads
/// behind a bounded queue. Renders run off the caller's scheduler; a full
/// queue refuses work instead of growing.
///
/// A renderer is trusted native code: the composer stops waiting at the
/// render budget and drops the result, but cannot stop a renderer that never
/// returns. Such a renderer holds its worker until it does.
pub struct PromptRenderPool {
    queue: SyncSender<RenderJob>,
    capacity: usize,
}

impl PromptRenderPool {
    /// A pool of up to `workers` threads behind a queue of `queue` jobs. A
    /// worker the host cannot start is logged and left out.
    pub fn new(workers: NonZeroUsize, queue: NonZeroUsize) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<RenderJob>(queue.get());
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..workers.get() {
            let receiver = Arc::clone(&receiver);
            if let Err(error) = std::thread::Builder::new()
                .name(format!("lash-prompt-render-{index}"))
                .spawn(move || work(&receiver))
            {
                tracing::error!(%error, "a prompt render worker did not start");
            }
        }
        Self {
            queue: sender,
            capacity: queue.get(),
        }
    }

    /// The process's shared pool: one worker per available core, up to
    /// eight, behind a queue of 1024 renders.
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<PromptRenderPool> = OnceLock::new();
        SHARED.get_or_init(|| {
            let workers = std::thread::available_parallelism()
                .map_or(1, NonZeroUsize::get)
                .clamp(1, 8);
            Self::new(
                NonZeroUsize::new(workers).unwrap_or(NonZeroUsize::MIN),
                NonZeroUsize::new(1024).unwrap_or(NonZeroUsize::MIN),
            )
        })
    }

    fn submit(&self, job: RenderJob) -> Result<(), PromptCompositionError> {
        self.queue.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) | TrySendError::Disconnected(_) => {
                PromptCompositionError::RenderersBusy {
                    capacity: u32::try_from(self.capacity).unwrap_or(u32::MAX),
                }
            }
        })
    }
}

/// A worker: run jobs until the pool is dropped.
fn work(receiver: &Mutex<Receiver<RenderJob>>) {
    loop {
        let job = receiver.lock_recover().recv();
        match job {
            Ok(job) => job(),
            Err(_) => return,
        }
    }
}

/// One call's composed prompt: the text each placement carries and the
/// snapshot that records how it was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedPrompt {
    /// The final text of every [`PromptPlacement::InitialInstructions`]
    /// section, in plan order, separated by [`PROMPT_SECTION_SEPARATOR`].
    /// `None` when no such section has text.
    pub initial_instructions: Option<String>,
    /// The same for [`PromptPlacement::CurrentContext`] sections.
    pub current_context: Option<String>,
    /// The version-1 snapshot: the resolved plan and every section's base
    /// text, wrapper outputs and final text.
    pub snapshot: PromptSnapshot,
    /// Every distinct text the snapshot references, by content address.
    pub texts: BTreeMap<BlobRef, String>,
}

impl ComposedPrompt {
    /// The owner-commit write that records this composition as `call`'s
    /// snapshot root over its content-shared texts. It belongs in the
    /// commit that admits the call, once per new call: a second record of
    /// one call is refused.
    ///
    /// # Errors
    ///
    /// The encoding error when the snapshot does not encode.
    pub fn record(&self, call: PromptCallKey) -> Result<DomainWrite, serde_json::Error> {
        Ok(DomainWrite::Prompt(PromptWrite::Record {
            call,
            snapshot: serde_json::to_string(&self.snapshot)?,
            texts: self
                .texts
                .iter()
                .map(|(hash, text)| PromptText {
                    hash: hash.as_str().to_owned(),
                    text: text.clone(),
                })
                .collect(),
        }))
    }
}

impl PromptCatalog {
    /// Compose a `purpose` call's prompt over `cut` under the host's `plan`:
    /// resolve the plan for the tools `cut` offers, render each section and its wrapper chain on
    /// `pool` within the plan's render budget, and assemble the result in
    /// plan order.
    ///
    /// # Errors
    ///
    /// [`PromptCompositionError`], attributed to the renderer or wrapper at
    /// fault where there is one. Nothing is composed: no partial text and no
    /// earlier text.
    pub async fn compose(
        &self,
        plan: &PromptPlan,
        purpose: &PromptPurpose,
        cut: Arc<PromptCut>,
        pool: &PromptRenderPool,
    ) -> Result<ComposedPrompt, PromptCompositionError> {
        let composition = Arc::new(
            self.resolve(plan, purpose, cut.offered())
                .map_err(|error| PromptCompositionError::Plan { error })?,
        );
        composition.render(cut, pool).await
    }
}

/// Stops queued renders of an abandoned composition from starting.
struct Abandon(Arc<AtomicBool>);

impl Drop for Abandon {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl ResolvedPromptComposition {
    async fn render(
        self: Arc<Self>,
        cut: Arc<PromptCut>,
        pool: &PromptRenderPool,
    ) -> Result<ComposedPrompt, PromptCompositionError> {
        let limits = self.record().limits;
        let budget_ms = limits.render_budget_ms.get();
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(u64::from(budget_ms));
        let abandoned = Arc::new(AtomicBool::new(false));
        // Every exit before assembly, the budget's included, abandons the
        // renders still queued; a running one finishes into a dropped
        // channel.
        let _abandon = Abandon(Arc::clone(&abandoned));
        let mut pending = Vec::with_capacity(self.record().sections.len());
        for index in 0..self.record().sections.len() {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let composition = Arc::clone(&self);
            let cut = Arc::clone(&cut);
            let abandoned = Arc::clone(&abandoned);
            pool.submit(Box::new(move || {
                if abandoned.load(Ordering::Acquire) {
                    return;
                }
                let _late_or_kept = sender.send(composition.compose_section(index, &cut));
            }))?;
            pending.push(receiver);
        }
        let mut composed = Vec::with_capacity(pending.len());
        for receiver in pending {
            match tokio::time::timeout_at(deadline, receiver).await {
                Ok(Ok(section)) => composed.push(section?),
                Ok(Err(_)) => {
                    return Err(PromptCompositionError::RenderersBusy {
                        capacity: u32::try_from(pool.capacity).unwrap_or(u32::MAX),
                    });
                }
                Err(_) => return Err(PromptCompositionError::BudgetExceeded { budget_ms }),
            }
        }

        let total = composed
            .iter()
            .map(|section| section.value.as_text().map_or(0, str::len) as u64)
            .sum::<u64>();
        let limit = limits.max_total_bytes.get();
        if total > u64::from(limit) {
            return Err(PromptCompositionError::TotalTooLarge {
                bytes: total,
                limit,
            });
        }

        let mut texts = BTreeMap::new();
        let mut keep = |text: &super::SectionText| {
            if let Some(text) = text.as_text() {
                texts
                    .entry(PromptTextRef::of(text).blob)
                    .or_insert_with(|| text.to_owned());
            }
        };
        let mut initial = Vec::new();
        let mut current = Vec::new();
        let mut sections = Vec::with_capacity(composed.len());
        for (resolved, section) in self.record().sections.iter().zip(&composed) {
            keep(&section.base);
            for (_, output) in &section.wraps {
                keep(output);
            }
            keep(&section.value);
            if let Some(text) = section.value.as_text() {
                match resolved.placement {
                    PromptPlacement::InitialInstructions => initial.push(text),
                    PromptPlacement::CurrentContext => current.push(text),
                    PromptPlacement::Excluded => {}
                }
            }
            sections.push(section.record(resolved));
        }
        let joined =
            |parts: Vec<&str>| (!parts.is_empty()).then(|| parts.join(PROMPT_SECTION_SEPARATOR));
        Ok(ComposedPrompt {
            initial_instructions: joined(initial),
            current_context: joined(current),
            snapshot: PromptSnapshot {
                version: PromptSnapshotVersion,
                plan: self.record().clone(),
                sections,
            },
            texts,
        })
    }
}

/// Why an admitted call's snapshot could not be read back.
#[derive(Debug, thiserror::Error)]
pub enum PromptSnapshotLoadError {
    #[error("prompt snapshot read: {0}")]
    Store(#[from] DurableError),
    #[error("prompt snapshot does not decode: {0}")]
    Decode(#[from] serde_json::Error),
    /// The snapshot references a text its root does not retain.
    #[error("prompt text {hash} is missing")]
    MissingText { hash: String },
    /// A stored text does not match its content address.
    #[error("prompt text {hash} does not match its address")]
    TextMismatch { hash: String },
}

/// An admitted call's snapshot, read back with every text it references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedPromptSnapshot {
    pub snapshot: PromptSnapshot,
    pub texts: BTreeMap<BlobRef, String>,
}

impl LoadedPromptSnapshot {
    /// The exact text `recorded` names, or `None` for an omission.
    pub fn text(&self, recorded: &RecordedSectionText) -> Option<&str> {
        match recorded {
            RecordedSectionText::Text { text } => self.texts.get(&text.blob).map(String::as_str),
            RecordedSectionText::Omitted => None,
        }
    }
}

/// Read `call`'s snapshot back from `reads`: the recorded snapshot and every
/// text it references, each verified against its content address. No
/// renderer or wrapper runs. `None` when the call has no retained snapshot.
///
/// # Errors
///
/// [`PromptSnapshotLoadError`] when the store fails, the snapshot does not
/// decode, or a text is missing or does not match its address.
pub async fn load_prompt_snapshot(
    reads: &dyn DurableReads,
    call: &PromptCallKey,
) -> Result<Option<LoadedPromptSnapshot>, PromptSnapshotLoadError> {
    let Some(row) = reads.prompt_snapshot(call).await? else {
        return Ok(None);
    };
    let snapshot: PromptSnapshot = serde_json::from_str(&row.snapshot)?;
    let mut texts = BTreeMap::new();
    for stored in reads.prompt_texts(&row.texts).await? {
        if PromptTextRef::of(&stored.text).blob.as_str() != stored.hash {
            return Err(PromptSnapshotLoadError::TextMismatch { hash: stored.hash });
        }
        texts.insert(BlobRef(stored.hash), stored.text);
    }
    let referenced = snapshot.sections.iter().flat_map(|section| {
        std::iter::once(&section.base)
            .chain(section.wraps.iter().map(|wrap| &wrap.output))
            .chain(std::iter::once(&section.value))
    });
    for recorded in referenced {
        if let RecordedSectionText::Text { text } = recorded
            && !texts.contains_key(&text.blob)
        {
            return Err(PromptSnapshotLoadError::MissingText {
                hash: text.blob.as_str().to_owned(),
            });
        }
    }
    Ok(Some(LoadedPromptSnapshot { snapshot, texts }))
}
