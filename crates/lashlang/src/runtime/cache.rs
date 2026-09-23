use crate::{
    HostRequirementsRef, LASHLANG_COMPILER_VERSION, LASHLANG_VM_ABI_VERSION,
    LashlangHostEnvironment, LinkError, LinkedModule, ModuleArtifact, ProcessRef, Program,
};

use super::entry_points::{Entry, compile, compile_main};
use super::{CompiledProgram, prewarm};
use rustc_hash::FxHasher;
use std::borrow::Borrow;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use thiserror::Error;

const DEFAULT_LINKED_PROGRAM_CACHE_CAPACITY: usize = 64;
const SOURCE_CACHE_VERSION: &str = "lashlang-source-v1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompiledProgramCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
    pub capacity: usize,
}

const DEFAULT_COMPILED_PROCESS_CACHE_CAPACITY: usize = 64;

/// The MRU bookkeeping the three caches in this file share: the entries
/// deque (the front is the eviction candidate, the back is most recently
/// used), the hit/miss/eviction counters, and the capacity-bound insert.
/// What an entry holds and how a lookup matches stay with each cache — this
/// type only owns the policy every cache applies identically.
struct MruEntries<E> {
    entries: VecDeque<E>,
    hits: u64,
    misses: u64,
    evictions: u64,
    capacity: usize,
}

impl<E> MruEntries<E> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            hits: 0,
            misses: 0,
            evictions: 0,
            capacity,
        }
    }

    /// A cache hit under the shared policy: probe the back entry, else find and promote the
    /// match to the back.
    #[expect(
        clippy::expect_used,
        reason = "the index came from position() over this same entries vec a few lines above"
    )]
    fn lookup(&mut self, matches: impl Fn(&E) -> bool) -> Option<&E> {
        if self.entries.back().is_some_and(&matches) {
            self.hits += 1;
            return self.entries.back();
        }
        let index = self.entries.iter().position(matches)?;
        self.hits += 1;
        let entry = self
            .entries
            .remove(index)
            .expect("cache index came from existing entry");
        self.entries.push_back(entry);
        self.entries.back()
    }

    fn miss(&mut self) {
        self.misses += 1;
    }

    /// Capacity zero bypasses residency entirely; at capacity the front entry is evicted.
    fn insert(&mut self, entry: E) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            self.evictions += 1;
        }
        self.entries.push_back(entry);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
    }

    fn stats(&self) -> CompiledProgramCacheStats {
        CompiledProgramCacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            entries: self.entries.len(),
            capacity: self.capacity,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompiledProcessCacheKey {
    pub module_ref: crate::ModuleRef,
    pub process_ref: ProcessRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub compiler_version: &'static str,
    pub vm_abi_version: &'static str,
}

/// Owned cache keys built so far. A hit compares borrowed fields, so this must
/// only ever advance on a miss.
#[cfg(test)]
pub(crate) static COMPILED_PROCESS_KEYS_BUILT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

impl CompiledProcessCacheKey {
    pub fn new(
        module_ref: crate::ModuleRef,
        process_ref: ProcessRef,
        host_requirements_ref: HostRequirementsRef,
    ) -> Self {
        #[cfg(test)]
        COMPILED_PROCESS_KEYS_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            module_ref,
            process_ref,
            host_requirements_ref,
            compiler_version: LASHLANG_COMPILER_VERSION,
            vm_abi_version: LASHLANG_VM_ABI_VERSION,
        }
    }

    /// A cache lookup only ever compares its key, so building an owned one to
    /// do it charges every hit for three string clones it immediately drops.
    fn matches(
        &self,
        module_ref: &crate::ModuleRef,
        process_ref: &ProcessRef,
        host_requirements_ref: &HostRequirementsRef,
    ) -> bool {
        self.compiler_version == LASHLANG_COMPILER_VERSION
            && self.vm_abi_version == LASHLANG_VM_ABI_VERSION
            && &self.module_ref == module_ref
            && &self.process_ref == process_ref
            && &self.host_requirements_ref == host_requirements_ref
    }
}

pub struct CompiledProcessCache {
    mru: MruEntries<CachedCompiledProcess>,
}

struct CachedCompiledProcess {
    key: CompiledProcessCacheKey,
    compiled: Arc<CompiledProgram>,
}

impl CompiledProcessCache {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_COMPILED_PROCESS_CACHE_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        prewarm();
        Self {
            mru: MruEntries::with_capacity(capacity),
        }
    }

    pub fn get_or_compile(
        &mut self,
        artifact: &ModuleArtifact,
        process_ref: &ProcessRef,
        host_requirements_ref: &HostRequirementsRef,
    ) -> Result<Arc<CompiledProgram>, crate::RuntimeError> {
        // Compare borrowed: a hit must not allocate a key it only reads.
        if let Some(entry) = self.mru.lookup(|entry| {
            entry
                .key
                .matches(artifact.module_ref(), process_ref, host_requirements_ref)
        }) {
            return Ok(entry.compiled.clone());
        }

        self.mru.miss();
        let compiled = Arc::new(compile(artifact, Entry::Process(process_ref), None)?);
        // Only a miss stores an entry, so only a miss pays for the owned key.
        self.mru.insert(CachedCompiledProcess {
            key: CompiledProcessCacheKey::new(
                artifact.module_ref().clone(),
                process_ref.clone(),
                host_requirements_ref.clone(),
            ),
            compiled: compiled.clone(),
        });
        Ok(compiled)
    }

    pub fn clear(&mut self) {
        self.mru.clear();
    }

    pub fn stats(&self) -> CompiledProgramCacheStats {
        self.mru.stats()
    }
}

impl Default for CompiledProcessCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LinkedProgramCacheError {
    #[error(transparent)]
    Link(#[from] LinkError),
}

#[derive(Debug)]
pub struct CompiledLinkedProgram {
    linked: LinkedModule,
    compiled: Arc<CompiledProgram>,
}

impl CompiledLinkedProgram {
    pub fn linked_module(&self) -> &LinkedModule {
        &self.linked
    }

    pub fn compiled_program(&self) -> &CompiledProgram {
        self.compiled.as_ref()
    }
}

pub struct LinkedProgramCache {
    mru: MruEntries<CachedLinkedProgram>,
}

struct CachedLinkedProgram {
    source_hash: u64,
    source: Arc<str>,
    process_handles: std::collections::BTreeSet<String>,
    program: Arc<CompiledLinkedProgram>,
}

impl LinkedProgramCache {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_LINKED_PROGRAM_CACHE_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        prewarm();
        Self {
            mru: MruEntries::with_capacity(capacity),
        }
    }

    /// Links and caches an already-parsed shared-AST program.
    ///
    /// The dialect front-end owns parsing (ADR 0096), so the cache is only ever
    /// handed a `Program`. A host should ask [`Self::cached_linked_program`]
    /// first, so that a hit does not pay for the parse this method's `program`
    /// argument required.
    pub fn get_or_compile_ast(
        &mut self,
        source: &str,
        program: Program,
        surface: impl Borrow<LashlangHostEnvironment>,
    ) -> Result<Arc<CompiledLinkedProgram>, LinkError> {
        let surface = surface.borrow();
        if let Some(program) = self.cached_linked_program(source, surface) {
            return Ok(program);
        }
        self.link_and_cache(source, program, surface)
    }

    /// The linked program already cached for this source and host
    /// surface, without parsing or linking anything.
    ///
    /// A hit is recorded and promoted exactly as it is on the compiling paths,
    /// so this is the lookup those paths use rather than a peek beside them.
    pub fn cached_linked_program(
        &mut self,
        source: &str,
        surface: impl Borrow<LashlangHostEnvironment>,
    ) -> Option<Arc<CompiledLinkedProgram>> {
        let source_hash = program_source_hash(source);
        let surface = surface.borrow();
        self.mru
            .lookup(|entry| linked_program_matches(entry, source_hash, source, surface))
            .map(|entry| entry.program.clone())
    }

    fn link_and_cache(
        &mut self,
        source: &str,
        program: Program,
        surface: &LashlangHostEnvironment,
    ) -> Result<Arc<CompiledLinkedProgram>, LinkError> {
        let source_hash = program_source_hash(source);
        self.mru.miss();
        let linked = LinkedModule::link(program, surface)?;
        let compiled = Arc::new(compile_main(&linked.artifact, Some(linked.spans())));
        let program = Arc::new(CompiledLinkedProgram { linked, compiled });
        self.mru.insert(CachedLinkedProgram {
            source_hash,
            source: Arc::<str>::from(source),
            process_handles: surface.process_handles.clone(),
            program: program.clone(),
        });
        Ok(program)
    }

    pub fn clear(&mut self) {
        self.mru.clear();
    }

    pub fn stats(&self) -> CompiledProgramCacheStats {
        self.mru.stats()
    }
}

impl Default for LinkedProgramCache {
    fn default() -> Self {
        Self::new()
    }
}

fn linked_program_matches(
    entry: &CachedLinkedProgram,
    source_hash: u64,
    source: &str,
    surface: &LashlangHostEnvironment,
) -> bool {
    source_matches(
        entry.source_hash,
        entry.source.as_ref(),
        source_hash,
        source,
    ) && entry.process_handles == surface.process_handles
        && surface.satisfies(entry.program.linked.artifact.host_requirements())
}

fn source_matches(cached_hash: u64, cached_source: &str, source_hash: u64, source: &str) -> bool {
    cached_hash == source_hash && cached_source == source
}

fn program_source_hash(source: &str) -> u64 {
    let mut hasher = FxHasher::default();
    SOURCE_CACHE_VERSION.hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}
