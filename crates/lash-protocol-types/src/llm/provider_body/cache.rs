//! What a provider lowered, kept by the retained identity of what it was
//! lowered from.

use super::RequestSegment;
use crate::allocation::{Allocated, AllocationBudget, AllocationError, AllocationHold};
use crate::llm::types::{LlmMessage, LlmRole};
use std::any::Any;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The bytes of segment text a cache keeps before it drops its least
/// recently used entries.
pub const SEGMENT_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// The provider lowering's request-level context identity.
/// version_surface = "coexist"
/// version_guard(items(SEGMENT_CONTEXT_DOMAIN, SegmentContextKey))
const SEGMENT_CONTEXT_DOMAIN: &str = "lash request segment context v1";

/// The identity of one lowered request segment within its context.
/// version_surface = "coexist"
/// version_guard(items(SEGMENT_KEY_DOMAIN, SegmentKey))
const SEGMENT_KEY_DOMAIN: &str = "lash request segment key v1";

/// The request-level inputs one provider's lowering reads (its model, the
/// session's attachment acceptance, dialect flags, codec revision): what a
/// message's lowering may depend on besides the message. Two calls whose
/// contexts differ share nothing.
///
/// ```
/// use lash_protocol_types::llm::types::SegmentContext;
///
/// let a = SegmentContext::new("example.provider/1").str("model-a").finish();
/// let b = SegmentContext::new("example.provider/1").str("model-b").finish();
/// assert_ne!(a, b);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SegmentContextKey([u8; 32]);

/// Builds a [`SegmentContextKey`] from length-delimited fields.
pub struct SegmentContext(blake3::Hasher);

impl SegmentContext {
    /// A context in `namespace`, a name and revision of the lowering that
    /// owns it: a lowering that changes what it writes changes its name.
    pub fn new(namespace: &str) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key(SEGMENT_CONTEXT_DOMAIN);
        hasher.update(&(namespace.len() as u64).to_be_bytes());
        hasher.update(namespace.as_bytes());
        Self(hasher)
    }

    #[must_use]
    pub fn str(mut self, value: &str) -> Self {
        self.0.update(&(value.len() as u64).to_be_bytes());
        self.0.update(value.as_bytes());
        self
    }

    #[must_use]
    pub fn u64(mut self, value: u64) -> Self {
        self.0.update(&value.to_be_bytes());
        self
    }

    /// `value`'s JSON, a field of the context.
    #[must_use]
    pub fn json(self, value: &impl serde::Serialize) -> Self {
        let text = serde_json::to_string(value).unwrap_or_default();
        self.str(&text)
    }

    pub fn finish(self) -> SegmentContextKey {
        SegmentContextKey(self.0.finalize().into())
    }
}

/// One lowered unit's key: its context, the messages it lowers, by
/// identity, and the modifiers its wire form reads (its position where a
/// provider numbers by position, where a cache breakpoint falls, mapped
/// ids). Build it, then hand it to [`SegmentCache::segments`].
pub struct SegmentKey {
    hasher: blake3::Hasher,
}

impl SegmentKey {
    pub fn new(context: &SegmentContextKey) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key(SEGMENT_KEY_DOMAIN);
        hasher.update(&context.0);
        Self { hasher }
    }

    /// A source message, by its never-reused block identity, role and
    /// user-segment marker. Naming it retains none of its source allocation.
    #[must_use]
    pub fn message(mut self, message: &LlmMessage) -> Self {
        self.hasher.update(&message.blocks.identity().to_be_bytes());
        self.hasher.update(&[
            match message.role {
                LlmRole::System => 0,
                LlmRole::User => 1,
                LlmRole::Assistant => 2,
            },
            u8::from(message.starts_user_segment),
        ]);
        self
    }

    #[must_use]
    pub fn u64(mut self, value: u64) -> Self {
        self.hasher.update(&value.to_be_bytes());
        self
    }

    #[must_use]
    pub fn str(mut self, value: &str) -> Self {
        self.hasher.update(&(value.len() as u64).to_be_bytes());
        self.hasher.update(value.as_bytes());
        self
    }
}

/// Reused lowerings: each unit a provider lowered, by its [`SegmentKey`],
/// for as long as the budget allows.
///
/// A hit returns the very segments built before, so what admission and
/// decoding derived from them ([`RequestSegment::derived`]) is reused too.
/// Nothing is invalidated explicitly: an edited, compacted or re-rendered
/// message is a new allocation and misses; another provider, model or
/// configuration is another context and misses. Clones share one cache.
///
/// ```
/// use lash_protocol_types::llm::types::{
///     LlmMessage, LlmRole, RequestSegment, SegmentCache, SegmentContext, SegmentKey,
/// };
///
/// let cache = SegmentCache::new();
/// let context = SegmentContext::new("example.provider/1").finish();
/// let message = LlmMessage::text(LlmRole::User, "hello");
/// let lower = |message: &LlmMessage| {
///     RequestSegment::item_json(&serde_json::json!({"text": "hello"})).map(|item| vec![item])
/// };
/// let first = cache
///     .segments(SegmentKey::new(&context).message(&message), 4096, |_| lower(&message))
///     .unwrap();
/// let again = cache
///     .segments(SegmentKey::new(&context).message(&message), 4096, |_| lower(&message))
///     .unwrap();
/// assert!(RequestSegment::same(&first[0], &again[0]));
/// assert_eq!(cache.stats().hits, 1);
/// ```
#[derive(Clone)]
pub struct SegmentCache(Option<Arc<Shared>>, AllocationBudget);

struct Shared {
    _metadata: crate::allocation::AllocationHold,
    budget: usize,
    entries: Mutex<Entries>,
    hits: AtomicU64,
    misses: AtomicU64,
}

#[derive(Clone, Default)]
struct Entries {
    map: BTreeMap<[u8; 32], Entry>,
    bytes: usize,
    clock: u64,
}

#[derive(Clone)]
struct Entry {
    _metadata: AllocationHold,
    value: Arc<dyn Any + Send + Sync>,
    bytes: usize,
    used: u64,
}

/// What a provider keeps for one lowered unit: its segments, and whatever
/// it reads back about them. Its size counts against the cache's budget.
pub trait LoweredUnit: Any + Send + Sync {
    /// The bytes of segment text this value keeps.
    fn retained_bytes(&self) -> usize;
}

impl LoweredUnit for Vec<RequestSegment> {
    fn retained_bytes(&self) -> usize {
        self.iter().map(RequestSegment::literal_len).sum()
    }
}

/// What a cache has served.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentCacheStats {
    pub hits: u64,
    pub misses: u64,
}

impl Default for SegmentCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SegmentCache {
    /// A cache keeping [`SEGMENT_CACHE_BUDGET_BYTES`].
    pub fn new() -> Self {
        Self::with_budget(SEGMENT_CACHE_BUDGET_BYTES)
    }

    /// A cache keeping `budget` bytes of segment text.
    pub fn with_budget(budget: usize) -> Self {
        Self(
            Some(Arc::new(Shared {
                _metadata: Default::default(),
                budget,
                entries: Mutex::new(Entries::default()),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            })),
            AllocationBudget::unowned(),
        )
    }

    /// A private candidate cache sharing immutable lowered units. Preparing a
    /// request cannot replace the acknowledged owner's source identity.
    pub fn draft(
        &self,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let Some(shared) = &self.0 else {
            return Ok(Self(None, budget.clone()));
        };
        let entries = shared.lock();
        let metadata = budget.reserve((std::mem::size_of::<Shared>() + 16) as u64)?;
        let mut copied = Entries {
            bytes: 0,
            clock: entries.clock,
            ..Default::default()
        };
        // An unowned/foreign lowering has no proof under this ledger. Cache
        // loss only rebuilds it; it must not import another owner's source.
        if self.1.identity() == budget.identity() {
            for (digest, entry) in &entries.map {
                let metadata = budget.reserve(ENTRY_ALLOCATION_BYTES)?;
                let mut entry = entry.clone();
                entry._metadata = metadata;
                copied.bytes = copied.bytes.saturating_add(entry.bytes);
                copied.map.insert(*digest, entry);
            }
        }
        Ok(Self(
            Some(Arc::new(Shared {
                _metadata: metadata,
                budget: shared.budget,
                entries: Mutex::new(copied),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            })),
            budget.clone(),
        ))
    }

    pub fn allocation_budget(&self) -> AllocationBudget {
        self.1.clone()
    }

    /// A cache that keeps nothing: every unit lowers afresh. For a call
    /// outside admission, whose lowering is never reused.
    pub fn disabled() -> Self {
        Self(None, AllocationBudget::unowned())
    }

    pub fn stats(&self) -> SegmentCacheStats {
        self.0
            .as_ref()
            .map_or_else(SegmentCacheStats::default, |shared| SegmentCacheStats {
                hits: shared.hits.load(Ordering::Relaxed),
                misses: shared.misses.load(Ordering::Relaxed),
            })
    }

    /// The segments `key` names: the ones lowered before, or what `lower`
    /// returns now, kept for the next call.
    ///
    /// # Errors
    ///
    /// What `lower` refuses with; a refusal is not kept.
    pub fn segments<E: From<AllocationError>>(
        &self,
        key: SegmentKey,
        allocation_bytes: u64,
        lower: impl FnOnce(AllocationHold) -> Result<Vec<RequestSegment>, E>,
    ) -> Result<Arc<Allocated<Vec<RequestSegment>>>, E> {
        self.lowered(key, allocation_bytes, lower)
    }

    /// Admit the callback's declared allocation bound before invoking it.
    /// The bound includes transient work and owned result fields; separately
    /// admitted immutable sources keep their own obligations. The callback
    /// must transfer the supplied hold into any nested escaping allocation.
    pub fn lowered<T: LoweredUnit, E: From<AllocationError>>(
        &self,
        key: SegmentKey,
        allocation_bytes: u64,
        lower: impl FnOnce(AllocationHold) -> Result<T, E>,
    ) -> Result<Arc<Allocated<T>>, E> {
        let digest = key.hasher.finalize().into();
        if let Some(value) = self.0.as_ref().and_then(|shared| shared.get(&digest)) {
            return Ok(value);
        }
        let budget = self.allocation_budget();
        let metadata = match &self.0 {
            Some(_) => budget.reserve(ENTRY_ALLOCATION_BYTES)?,
            None => AllocationHold::default(),
        };
        let hold = budget.reserve(
            allocation_bytes.saturating_add((std::mem::size_of::<Allocated<T>>() + 16) as u64),
        )?;
        let value = Arc::new(Allocated::new(lower(hold.clone())?, hold, budget));
        if let Some(shared) = &self.0 {
            shared.insert(digest, &value, metadata);
        }
        Ok(value)
    }

    /// A carried update reads an immutable snapshot outside the cache lock.
    /// Its reservation precedes the callback and belongs to the actual Arc
    /// returned to readers. Concurrent branches can publish independently.
    pub(super) fn carried<T: LoweredUnit, E: From<AllocationError>>(
        &self,
        key: SegmentKey,
        allocation_bytes: u64,
        update: impl FnOnce(Option<Arc<Allocated<T>>>, AllocationHold) -> Result<T, E>,
    ) -> Result<Arc<Allocated<T>>, E> {
        let digest = key.hasher.finalize().into();
        let budget = self.allocation_budget();
        let metadata = match &self.0 {
            Some(_) => budget.reserve(ENTRY_ALLOCATION_BYTES)?,
            None => AllocationHold::default(),
        };
        let hold = budget.reserve(
            allocation_bytes.saturating_add((std::mem::size_of::<Allocated<T>>() + 16) as u64),
        )?;
        let prior = self.0.as_ref().and_then(|shared| shared.get(&digest));
        let value = Arc::new(Allocated::new(update(prior, hold.clone())?, hold, budget));
        if let Some(shared) = &self.0 {
            shared.insert(digest, &value, metadata);
        }
        Ok(value)
    }

    /// No cache lock spans I/O. Dropping the future before publication drops
    /// the same reservations acquired before entering the callback.
    pub async fn carried_async<
        T: LoweredUnit,
        E: From<AllocationError>,
        F: std::future::Future<Output = Result<T, E>>,
    >(
        &self,
        key: SegmentKey,
        allocation_bytes: u64,
        update: impl FnOnce(Option<Arc<Allocated<T>>>, AllocationHold) -> F,
    ) -> Result<Arc<Allocated<T>>, E> {
        let digest = key.hasher.finalize().into();
        let budget = self.allocation_budget();
        let metadata = match &self.0 {
            Some(_) => budget.reserve(ENTRY_ALLOCATION_BYTES)?,
            None => AllocationHold::default(),
        };
        let hold = budget.reserve(
            allocation_bytes.saturating_add((std::mem::size_of::<Allocated<T>>() + 16) as u64),
        )?;
        let prior = self.0.as_ref().and_then(|shared| shared.get(&digest));
        let value = Arc::new(Allocated::new(
            update(prior, hold.clone()).await?,
            hold,
            budget,
        ));
        if let Some(shared) = &self.0 {
            shared.insert(digest, &value, metadata);
        }
        Ok(value)
    }
}

// A full sparse B-tree node, its key/value slots and allocator metadata.
const ENTRY_ALLOCATION_BYTES: u64 = 2048;

impl Shared {
    fn get<T: LoweredUnit>(&self, digest: &[u8; 32]) -> Option<Arc<Allocated<T>>> {
        let mut entries = self.lock();
        entries.clock = entries.clock.wrapping_add(1);
        let clock = entries.clock;
        if let Some(entry) = entries.map.get_mut(digest)
            && let Ok(value) = Arc::clone(&entry.value).downcast::<Allocated<T>>()
        {
            entry.used = clock;
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(value);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn insert<T: LoweredUnit>(
        &self,
        digest: [u8; 32],
        value: &Arc<Allocated<T>>,
        metadata: AllocationHold,
    ) {
        let bytes = value.retained_bytes().max(1);
        if bytes > self.budget {
            return;
        }
        let mut entries = self.lock();
        entries.clock = entries.clock.wrapping_add(1);
        let used = entries.clock;
        let replaced = entries.map.insert(
            digest,
            Entry {
                _metadata: metadata,
                value: Arc::clone(value) as Arc<dyn Any + Send + Sync>,
                bytes,
                used,
            },
        );
        entries.bytes = entries.bytes.saturating_add(bytes);
        if let Some(replaced) = &replaced {
            entries.bytes = entries.bytes.saturating_sub(replaced.bytes);
        }
        let target = if entries.bytes > self.budget {
            self.budget / 4 * 3
        } else {
            self.budget
        };
        drop(entries);
        drop(replaced);
        // No scratch directory or eviction vector: each removal is decided
        // under the synchronization owner, then released outside it.
        loop {
            let mut entries = self.lock();
            if entries.bytes <= target {
                break;
            }
            let oldest = entries
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(digest, _)| *digest);
            let evicted = oldest.and_then(|digest| entries.map.remove(&digest));
            if let Some(evicted) = &evicted {
                entries.bytes = entries.bytes.saturating_sub(evicted.bytes);
            }
            drop(entries);
            drop(evicted);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for SegmentCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentCache")
            .field("enabled", &self.0.is_some())
            .field("stats", &self.stats())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn dropping_an_async_lowering_releases_its_preadmitted_growth() {
        use crate::allocation::{AllocationError, testing::Ledger};
        use std::future::Future;
        let ledger = Ledger::default();
        ledger.limit(u64::MAX);
        let cache = SegmentCache::new().draft(&ledger.budget()).unwrap();
        let before = ledger.used();
        let context = SegmentContext::new("cache-cancel").finish();
        let mut future =
            Box::pin(
                cache.carried_async(SegmentKey::new(&context), 8192, |_, _| async {
                    std::future::pending::<()>().await;
                    Ok::<_, AllocationError>(Vec::<RequestSegment>::new())
                }),
            );
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(future.as_mut().poll(&mut task).is_pending());
        assert!(
            ledger.used() >= before + 8192,
            "the callback's claimed region must already own its reservation"
        );
        drop(future);
        assert_eq!(
            ledger.used(),
            before,
            "cancellation releases every preclaimed obligation"
        );
        drop(cache);
        assert_eq!(ledger.used(), 0);
    }

    #[test]
    fn segment_identity_does_not_retain_source_blocks() {
        let cache = SegmentCache::new();
        let context = SegmentContext::new("source-lifetime").finish();
        let source = LlmMessage::text(LlmRole::User, "source");
        let first = cache
            .segments(SegmentKey::new(&context).message(&source), 4096, |hold| {
                Ok::<_, crate::allocation::AllocationError>(vec![
                    RequestSegment::item_json(&serde_json::json!("first"))
                        .unwrap()
                        .with_allocation(hold),
                ])
            })
            .unwrap();
        assert_eq!(
            Arc::strong_count(&source.blocks),
            1,
            "cache identity must not keep source blocks alive"
        );
        let same = cache
            .segments(
                SegmentKey::new(&context).message(&source),
                4096,
                |_| -> Result<_, crate::allocation::AllocationError> { unreachable!() },
            )
            .unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        drop(source);
        let next = LlmMessage::text(LlmRole::User, "source");
        let fresh = cache
            .segments(SegmentKey::new(&context).message(&next), 4096, |hold| {
                Ok::<_, crate::allocation::AllocationError>(vec![
                    RequestSegment::item_json(&serde_json::json!("fresh"))
                        .unwrap()
                        .with_allocation(hold),
                ])
            })
            .unwrap();
        assert!(
            !Arc::ptr_eq(&first, &fresh),
            "a fresh allocation has a fresh identity even for equal content"
        );
    }

    #[test]
    fn cache_refuses_growth_before_calling_the_lowerer() {
        use crate::allocation::{AllocationError, testing::Ledger};
        let ledger = Ledger::default();
        ledger.limit(u64::MAX);
        let cache = SegmentCache::new().draft(&ledger.budget()).unwrap();
        ledger.limit(ledger.used());
        let context = SegmentContext::new("cache-admission").finish();
        let source = LlmMessage::text(LlmRole::User, "source");
        let called = std::sync::atomic::AtomicBool::new(false);
        let result = cache.segments(SegmentKey::new(&context).message(&source), 4096, |hold| {
            called.store(true, Ordering::SeqCst);
            Ok::<_, AllocationError>(vec![
                RequestSegment::item_json(&serde_json::json!("lowered"))
                    .unwrap()
                    .with_allocation(hold),
            ])
        });
        assert!(
            result.is_err(),
            "new lowering and cache metadata require admission"
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "refusal precedes the callback"
        );
        drop(cache);
        assert_eq!(ledger.used(), 0);
    }

    #[test]
    fn concurrent_admitted_updates_keep_reader_snapshots_and_escaped_segments_charged() {
        use crate::allocation::testing::Ledger;
        let ledger = Ledger::default();
        ledger.limit(u64::MAX);
        let cache = SegmentCache::new().draft(&ledger.budget()).unwrap();
        let context = SegmentContext::new("admitted-carried-race").finish();
        let key = || SegmentKey::new(&context);
        let initial = cache
            .carried(key(), 4096, |_, hold| {
                Ok::<_, AllocationError>(vec![
                    RequestSegment::item_json(&serde_json::json!("initial"))
                        .unwrap()
                        .with_allocation(hold),
                ])
            })
            .unwrap();
        let barrier = Barrier::new(2);
        let update = |text: &str| {
            cache
                .carried(key(), 4096, |prior, hold| {
                    assert!(Arc::ptr_eq(&prior.unwrap(), &initial));
                    barrier.wait();
                    Ok::<_, AllocationError>(vec![
                        RequestSegment::item_json(&serde_json::json!(text))
                            .unwrap()
                            .with_allocation(hold),
                    ])
                })
                .unwrap()
        };
        let (left, right) = std::thread::scope(|scope| {
            let left = scope.spawn(|| update("left"));
            let right = scope.spawn(|| update("right"));
            (left.join().unwrap(), right.join().unwrap())
        });
        assert_eq!(initial[0].text_lossy(), "\"initial\"");
        assert_eq!(left[0].text_lossy(), "\"left\"");
        assert_eq!(right[0].text_lossy(), "\"right\"");
        let escaped = left[0].clone();
        drop(cache);
        drop(initial);
        drop(left);
        drop(right);
        assert!(
            ledger.used() >= 4096,
            "a segment can outlive the cache and its returned unit"
        );
        drop(escaped);
        assert_eq!(ledger.used(), 0);
    }
}
