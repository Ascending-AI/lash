//! What a provider lowered, kept by the retained identity of what it was
//! lowered from.

use super::RequestSegment;
use crate::llm::types::{LlmMessage, LlmRole};
use std::any::Any;
use std::collections::HashMap;
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
pub struct SegmentKey<'a> {
    hasher: blake3::Hasher,
    messages: Vec<&'a Arc<crate::llm::types::LlmMessageBlocks>>,
}

impl<'a> SegmentKey<'a> {
    pub fn new(context: &SegmentContextKey) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key(SEGMENT_KEY_DOMAIN);
        hasher.update(&context.0);
        Self {
            hasher,
            messages: Vec::new(),
        }
    }

    /// A source message, by the identity of its shared blocks, its role and
    /// its user-segment marker. The cache holds the blocks while the entry
    /// lives, so the identity cannot be reused for other content.
    #[must_use]
    pub fn message(mut self, message: &'a LlmMessage) -> Self {
        let address = Arc::as_ptr(&message.blocks) as usize as u64;
        self.hasher.update(&address.to_be_bytes());
        self.hasher.update(&[
            match message.role {
                LlmRole::System => 0,
                LlmRole::User => 1,
                LlmRole::Assistant => 2,
            },
            u8::from(message.starts_user_segment),
        ]);
        self.messages.push(&message.blocks);
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
///     .segments(SegmentKey::new(&context).message(&message), || lower(&message))
///     .unwrap();
/// let again = cache
///     .segments(SegmentKey::new(&context).message(&message), || lower(&message))
///     .unwrap();
/// assert!(RequestSegment::same(&first[0], &again[0]));
/// assert_eq!(cache.stats().hits, 1);
/// ```
#[derive(Clone)]
pub struct SegmentCache(Option<Arc<Shared>>);

struct Shared {
    _metadata: crate::allocation::AllocationHold,
    budget: usize,
    entries: Mutex<Entries>,
    hits: AtomicU64,
    misses: AtomicU64,
}

#[derive(Clone, Default)]
struct Entries {
    map: HashMap<[u8; 32], Entry>,
    bytes: usize,
    clock: u64,
}

#[derive(Clone)]
struct Entry {
    sources: Box<[Arc<crate::llm::types::LlmMessageBlocks>]>,
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
        Self(Some(Arc::new(Shared {
            _metadata: Default::default(),
            budget,
            entries: Mutex::new(Entries::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })))
    }

    /// A private candidate cache sharing immutable lowered units. Preparing a
    /// request cannot replace the acknowledged owner's source identity.
    pub fn draft(
        &self,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let Some(shared) = &self.0 else {
            return Ok(Self::disabled());
        };
        let entries = shared.lock();
        let bytes = std::mem::size_of::<Shared>()
            + 16
            + entries.map.capacity().max(4) * (std::mem::size_of::<Entry>() + 32 + 24)
            + entries
                .map
                .values()
                .map(|entry| {
                    entry.sources.len()
                        * std::mem::size_of::<Arc<crate::llm::types::LlmMessageBlocks>>()
                })
                .sum::<usize>();
        let metadata = budget.reserve(bytes as u64)?;
        Ok(Self(Some(Arc::new(Shared {
            _metadata: metadata,
            budget: shared.budget,
            entries: Mutex::new(entries.clone()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }))))
    }

    /// A cache that keeps nothing: every unit lowers afresh. For a call
    /// outside admission, whose lowering is never reused.
    pub fn disabled() -> Self {
        Self(None)
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
    pub fn segments<E>(
        &self,
        key: SegmentKey<'_>,
        lower: impl FnOnce() -> Result<Vec<RequestSegment>, E>,
    ) -> Result<Arc<Vec<RequestSegment>>, E> {
        self.lowered(key, lower)
    }

    /// [`Self::segments`] for a unit that keeps more than its segments.
    ///
    /// # Errors
    ///
    /// What `lower` refuses with; a refusal is not kept.
    pub fn lowered<T: LoweredUnit, E>(
        &self,
        key: SegmentKey<'_>,
        lower: impl FnOnce() -> Result<T, E>,
    ) -> Result<Arc<T>, E> {
        let Some(shared) = &self.0 else {
            return lower().map(Arc::new);
        };
        let SegmentKey { hasher, messages } = key;
        let digest: [u8; 32] = hasher.finalize().into();
        if let Some(value) = shared.get(&digest, &messages) {
            return Ok(value);
        }
        let value = Arc::new(lower()?);
        shared.insert(digest, messages, &value);
        Ok(value)
    }

    /// Carry a lowering across calls under one retained source identity.
    /// Updating works on an immutable snapshot outside the cache lock. A
    /// concurrent call may publish another branch; the next update checks
    /// its sources again, so publication order cannot supply stale content.
    pub(super) fn carried<T: LoweredUnit, E>(
        &self,
        key: SegmentKey<'_>,
        update: impl FnOnce(Option<Arc<T>>) -> Result<Arc<T>, E>,
    ) -> Result<Arc<T>, E> {
        let Some(shared) = &self.0 else {
            return update(None);
        };
        let SegmentKey { hasher, messages } = key;
        let digest = hasher.finalize().into();
        let prior = shared.get(&digest, &messages);
        let value = update(prior.clone())?;
        if !prior
            .as_ref()
            .is_some_and(|prior| Arc::ptr_eq(prior, &value))
        {
            shared.insert(digest, messages, &value);
        }
        Ok(value)
    }
    /// An asynchronous provider's carried history; no cache lock spans I/O.
    pub async fn carried_async<
        T: LoweredUnit,
        E,
        F: std::future::Future<Output = Result<Arc<T>, E>>,
    >(
        &self,
        key: SegmentKey<'_>,
        update: impl FnOnce(Option<Arc<T>>) -> F,
    ) -> Result<Arc<T>, E> {
        let Some(shared) = &self.0 else {
            return update(None).await;
        };
        let SegmentKey { hasher, messages } = key;
        let digest = hasher.finalize().into();
        let prior = shared.get(&digest, &messages);
        let value = update(prior).await?;
        shared.insert(digest, messages, &value);
        Ok(value)
    }
}

impl Shared {
    fn get<T: LoweredUnit>(
        &self,
        digest: &[u8; 32],
        messages: &[&Arc<crate::llm::types::LlmMessageBlocks>],
    ) -> Option<Arc<T>> {
        let mut entries = self.lock();
        entries.clock += 1;
        let clock = entries.clock;
        if let Some(entry) = entries.map.get_mut(digest)
            && entry.sources.len() == messages.len()
            && entry
                .sources
                .iter()
                .zip(messages)
                .all(|(held, message)| Arc::ptr_eq(held, message))
            && let Ok(value) = Arc::clone(&entry.value).downcast::<T>()
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
        messages: Vec<&Arc<crate::llm::types::LlmMessageBlocks>>,
        value: &Arc<T>,
    ) {
        let bytes = value.retained_bytes().max(1);
        if bytes > self.budget {
            return;
        }
        let mut entries = self.lock();
        entries.clock += 1;
        let used = entries.clock;
        let replaced = entries.map.insert(
            digest,
            Entry {
                sources: messages.into_iter().cloned().collect(),
                value: Arc::clone(value) as Arc<dyn Any + Send + Sync>,
                bytes,
                used,
            },
        );
        entries.bytes = entries.bytes.saturating_add(bytes);
        if let Some(replaced) = &replaced {
            entries.bytes = entries.bytes.saturating_sub(replaced.bytes);
        }
        let evicted = if entries.bytes > self.budget {
            entries.evict(self.budget / 4 * 3)
        } else {
            Vec::new()
        };
        drop(entries);
        // A carried history can release many source and segment Arcs.
        // Release them after relinquishing the cache's synchronization owner.
        drop(replaced);
        drop(evicted);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Entries {
    /// Drop the least recently used entries until at most `target` bytes
    /// remain.
    fn evict(&mut self, target: usize) -> Vec<Entry> {
        let mut evicted = Vec::new();
        let mut order: Vec<([u8; 32], u64)> = self
            .map
            .iter()
            .map(|(digest, entry)| (*digest, entry.used))
            .collect();
        order.sort_unstable_by_key(|(_, used)| *used);
        for (digest, _) in order {
            if self.bytes <= target {
                break;
            }
            if let Some(entry) = self.map.remove(&digest) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
                evicted.push(entry);
            }
        }
        evicted
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
    fn racing_carried_lowerings_keep_each_call_and_prior_snapshot_immutable() {
        let cache = SegmentCache::new();
        let context = SegmentContext::new("carried-race").finish();
        let source = LlmMessage::text(LlmRole::User, "shared source");
        let key = || SegmentKey::new(&context).message(&source);
        let unit = |text: &str| {
            Arc::new(vec![
                RequestSegment::item_json(&serde_json::json!(text)).unwrap(),
            ])
        };
        let original = unit("original");
        cache
            .carried(key(), |_| Ok::<_, ()>(Arc::clone(&original)))
            .unwrap();
        let barrier = Barrier::new(2);
        let update = |text| {
            let own = unit(text);
            let returned = cache
                .carried(key(), |prior| {
                    assert!(Arc::ptr_eq(&prior.unwrap(), &original));
                    // Both calls captured the same prior before either publishes.
                    barrier.wait();
                    Ok::<_, ()>(Arc::clone(&own))
                })
                .unwrap();
            assert!(Arc::ptr_eq(&returned, &own));
            own
        };
        let (left, right) = std::thread::scope(|scope| {
            let left = scope.spawn(|| update("left branch"));
            let right = scope.spawn(|| update("right branch"));
            (left.join().unwrap(), right.join().unwrap())
        });
        let current = cache
            .carried(key(), |prior| Ok::<_, ()>(prior.unwrap()))
            .unwrap();
        assert!(Arc::ptr_eq(&current, &left) || Arc::ptr_eq(&current, &right));
        assert_eq!(original[0].text_lossy(), "\"original\"");
        assert_eq!(left[0].text_lossy(), "\"left branch\"");
        assert_eq!(right[0].text_lossy(), "\"right branch\"");
    }
}
