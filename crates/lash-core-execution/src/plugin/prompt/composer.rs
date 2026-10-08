//! The prompt composer (FIG-5256, ADR 0133 §5 to §7): the one route from a
//! session's registered sections to a call's instruction text and its
//! snapshot.
//!
//! [`PromptCatalog::compose`], which the runtime reaches only through
//! [`core_internal::compose_prompt`](crate::core_internal::compose_prompt),
//! resolves the host's plan, renders every section's base text and then its
//! wrapper chain on a bounded [`PromptRenderPool`], and assembles the result
//! in plan order, never in completion order. It fails closed: a refusal, panic, oversize text or a
//! render that outlasts the plan's budget composes nothing, and no earlier
//! text stands in. A late render's result is dropped.
//!
//! A call's admission ([`admission_record`]) records its [`ComposedPrompt`],
//! its request template and its response context as one root whose texts and literal chunks are
//! stored by content address, so a section that did not change between
//! calls, or a literal prefix two calls share, is stored once. The template's
//! attachment slots record refs, never a delivered value (ADR 0135 §6).
//! [`load_admitted_call`] reads an admitted call back, every text verified
//! against its address, without calling any renderer, wrapper or provider
//! builder.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Barrier, Mutex, OnceLock};

use lash_durable::domain::{PromptCallKey, PromptText, PromptWrite};
use lash_durable::{DomainWrite, DurableError, DurableInstant, DurableReads};
use lash_sansio::llm::types::{
    LlmRequestScope, RecordedRequestTemplate, ResponseContext, ResponseContract,
};
use lash_sansio::sync::MutexExt;

use super::{
    PromptCatalog, PromptCompositionError, PromptCut, PromptPlacement, PromptPlan, PromptPurpose,
    PromptSnapshot, PromptSnapshotVersion, PromptTextRef, RecordedSectionText,
    ResolvedPromptComposition,
};
use crate::prompt_sections::{
    AdmittedModelCall, ChunkedRequestTemplate, ProviderBodyError, RecordedResponseContext,
};
use crate::store::BlobRef;

/// What separates two sections' text within one placement.
pub const PROMPT_SECTION_SEPARATOR: &str = "\n\n";

/// Host capacity for prompt composition; operational bounds rather than refusal ceilings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptRenderPoolConfig {
    pub workers: NonZeroUsize,
    pub queue: NonZeroUsize,
}
impl Default for PromptRenderPoolConfig {
    fn default() -> Self {
        Self::standard()
    }
}
impl PromptRenderPoolConfig {
    /// Standard preset: one worker per available core, at most eight (one if
    /// unavailable), and a 1,024-job queue. Historical, unmeasured capacities.
    pub fn standard() -> Self {
        Self {
            workers: std::thread::available_parallelism()
                .unwrap_or(NonZeroUsize::MIN)
                .min(NonZeroUsize::MIN.saturating_add(7)),
            queue: NonZeroUsize::MIN.saturating_add(1023),
        }
    }
}

type RenderJob = Box<dyn FnOnce() + Send>;

/// Renders submitted to any pool of the process and not yet ended.
static RENDERS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Renders of the process that ended, run or dropped unrun.
static RENDERS_ENDED: AtomicUsize = AtomicUsize::new(0);

/// One submitted render: it ends when its job has run, or is dropped unrun.
struct RenderInFlight;

impl RenderInFlight {
    fn begin() -> Self {
        RENDERS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for RenderInFlight {
    fn drop(&mut self) {
        RENDERS_ENDED.fetch_add(1, Ordering::SeqCst);
        RENDERS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

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
    pub fn from_config(config: PromptRenderPoolConfig) -> Self {
        Self::new(config.workers, config.queue)
    }

    /// A pool of up to `workers` threads behind a queue of `queue` jobs. A
    /// worker the host cannot start is logged and left out. Every started
    /// worker has entered its thread before the pool returns.
    pub fn new(workers: NonZeroUsize, queue: NonZeroUsize) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<RenderJob>(queue.get());
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..workers.get() {
            let receiver = Arc::clone(&receiver);
            let ready = Arc::new(Barrier::new(2));
            let worker_ready = Arc::clone(&ready);
            match std::thread::Builder::new()
                .name(format!("lash-prompt-render-{index}"))
                .spawn(move || {
                    // Thread startup has installed the worker's native name.
                    // Pool initialization is complete only past this point.
                    worker_ready.wait();
                    work(&receiver);
                }) {
                Ok(_) => {
                    ready.wait();
                }
                Err(error) => tracing::error!(%error, "a prompt render worker did not start"),
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
        SHARED.get_or_init(|| Self::from_config(PromptRenderPoolConfig::standard()))
    }

    /// Whether a render of any pool of the process is in flight, and how
    /// many have ended: renders run off the caller's runtime, so a
    /// simulation on a virtual clock waits them out before its clock moves.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn renders_in_flight() -> (bool, usize) {
        (
            RENDERS_IN_FLIGHT.load(Ordering::SeqCst) > 0,
            RENDERS_ENDED.load(Ordering::SeqCst),
        )
    }

    fn submit(&self, job: RenderJob) -> Result<(), PromptCompositionError> {
        let render = RenderInFlight::begin();
        let job: RenderJob = Box::new(move || {
            job();
            drop(render);
        });
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

/// The owner-commit write that admits `call` (ADR 0133 §6): one root holding
/// its record, an [`AdmittedModelCall`] of `prompt`'s snapshot (none when the
/// session registers no sections), the request `template`, the scope and
/// contract of `response` (what every send of the call reads its response
/// under; its senders are live and not recorded) and, for a call no turn row
/// pins, its `deadline`, over every section text, literal chunk and the
/// contract stored by content. It belongs in the commit that admits the
/// call, once per new call: a second record of one call is refused.
///
/// # Errors
///
/// The encoding error when the record does not encode.
pub fn admission_record(
    call: PromptCallKey,
    prompt: Option<&ComposedPrompt>,
    template: &RecordedRequestTemplate,
    response: &ResponseContext,
    deadline: Option<DurableInstant>,
) -> Result<DomainWrite, serde_json::Error> {
    let (recorded_body, chunks) = ChunkedRequestTemplate::chunk(template);
    let mut texts: BTreeMap<BlobRef, String> = chunks.into_iter().collect();
    let (recorded_response, contract) = RecordedResponseContext::chunk(
        LlmRequestScope {
            // The attempt is each send's own.
            attempt: None,
            ..response.scope.clone()
        },
        &serde_json::to_string(response.contract.as_ref())?,
    );
    texts.extend(contract);
    if let Some(prompt) = prompt {
        texts.extend(
            prompt
                .texts
                .iter()
                .map(|(hash, text)| (hash.clone(), text.clone())),
        );
    }
    let record = AdmittedModelCall {
        version: PromptSnapshotVersion,
        prompt: prompt.map(|prompt| prompt.snapshot.clone()),
        body: recorded_body,
        response: recorded_response,
        deadline_ms: deadline.map(|deadline| deadline.0),
    };
    Ok(DomainWrite::Prompt(PromptWrite::Record {
        call,
        snapshot: serde_json::to_string(&record)?,
        texts: texts
            .into_iter()
            .map(|(hash, text)| PromptText {
                hash: hash.as_str().to_owned(),
                text,
            })
            .collect(),
    }))
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
    pub(crate) async fn compose(
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

/// Why an admitted call could not be read back as it was admitted.
#[derive(Debug, thiserror::Error)]
pub enum AdmittedCallLoadError {
    #[error("admitted call read: {0}")]
    Store(#[from] DurableError),
    #[error("admitted call does not decode: {0}")]
    Decode(#[from] serde_json::Error),
    /// The snapshot references a text its root does not retain.
    #[error("prompt text {hash} is missing")]
    MissingText { hash: String },
    /// A stored text does not match its content address.
    #[error("prompt text {hash} does not match its address")]
    TextMismatch { hash: String },
    /// The template cannot be assembled from its stored chunks.
    #[error("admitted request template: {0}")]
    Body(#[from] ProviderBodyError),
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

/// An admitted call, read back: its prompt snapshot, the request template
/// every send of it fills and sends, the scope and contract every send
/// reads its response under, and an owned call's pinned deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedAdmittedCall {
    /// `None` when the session registered no sections.
    pub prompt: Option<LoadedPromptSnapshot>,
    pub template: Arc<RecordedRequestTemplate>,
    pub scope: LlmRequestScope,
    pub contract: Arc<ResponseContract>,
    pub deadline: Option<DurableInstant>,
}

impl LoadedAdmittedCall {
    /// The call's response context as its admission recorded it, with no
    /// senders: a send adds its own.
    pub fn response(&self) -> ResponseContext {
        ResponseContext::recorded(self.scope.clone(), Arc::clone(&self.contract))
    }
}

/// Read `call`'s admission back from `reads`: its record, every section text
/// and literal chunk it references, each verified against its content
/// address, and the template assembled byte for byte. No renderer, wrapper or provider
/// builder runs. `None` when the call has no retained root.
///
/// # Errors
///
/// [`AdmittedCallLoadError`] when the store fails, the record does not
/// decode, a text is missing or does not match its address, the template
/// does not assemble as recorded, or the response contract does not decode.
pub async fn load_admitted_call(
    reads: &dyn DurableReads,
    call: &PromptCallKey,
) -> Result<Option<LoadedAdmittedCall>, AdmittedCallLoadError> {
    let Some(row) = reads.prompt_snapshot(call).await? else {
        return Ok(None);
    };
    let admitted: AdmittedModelCall = serde_json::from_str(&row.snapshot)?;
    let mut texts = BTreeMap::new();
    for stored in reads.prompt_texts(&row.texts).await? {
        if PromptTextRef::of(&stored.text).blob.as_str() != stored.hash {
            return Err(AdmittedCallLoadError::TextMismatch { hash: stored.hash });
        }
        texts.insert(BlobRef(stored.hash), stored.text);
    }
    let template = Arc::new(
        admitted
            .body
            .assemble(|blob| texts.get(blob).map(String::as_str))?,
    );
    let contract = admitted
        .response
        .assemble(|blob| texts.get(blob).map(String::as_str))?;
    let contract: Arc<ResponseContract> = Arc::new(serde_json::from_str(&contract)?);
    let prompt = match admitted.prompt {
        Some(snapshot) => {
            let referenced = snapshot.sections.iter().flat_map(|section| {
                std::iter::once(&section.base)
                    .chain(section.wraps.iter().map(|wrap| &wrap.output))
                    .chain(std::iter::once(&section.value))
            });
            // The snapshot's own texts: the template's chunks share the root.
            let mut prompt_texts = BTreeMap::new();
            for recorded in referenced {
                if let RecordedSectionText::Text { text } = recorded {
                    let Some(stored) = texts.get(&text.blob) else {
                        return Err(AdmittedCallLoadError::MissingText {
                            hash: text.blob.as_str().to_owned(),
                        });
                    };
                    prompt_texts.insert(text.blob.clone(), stored.clone());
                }
            }
            Some(LoadedPromptSnapshot {
                snapshot,
                texts: prompt_texts,
            })
        }
        None => None,
    };
    Ok(Some(LoadedAdmittedCall {
        prompt,
        template,
        scope: admitted.response.scope,
        contract,
        deadline: admitted.deadline_ms.map(DurableInstant),
    }))
}
