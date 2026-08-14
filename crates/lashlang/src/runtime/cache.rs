use crate::{
    CompilationDialect, HostRequirementsRef, LASHLANG_COMPILER_VERSION, LASHLANG_VM_ABI_VERSION,
    LashlangHostEnvironment, LinkError, LinkedModule, ModuleArtifact, ProcessRef, Program,
};

use super::entry_points::{
    compile_linked_with_dialect, compile_module_artifact_process, compile_program_internal,
};
use super::{CompiledProgram, prewarm};
use rustc_hash::FxHasher;
use std::borrow::Borrow;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use thiserror::Error;

const DEFAULT_COMPILED_PROGRAM_CACHE_CAPACITY: usize = 64;
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompiledProcessCacheKey {
    pub module_ref: crate::ModuleRef,
    pub process_ref: ProcessRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub compilation_dialect: CompilationDialect,
    pub compiler_version: &'static str,
    pub vm_abi_version: &'static str,
}

impl CompiledProcessCacheKey {
    pub fn new(
        module_ref: crate::ModuleRef,
        process_ref: ProcessRef,
        host_requirements_ref: HostRequirementsRef,
        compilation_dialect: CompilationDialect,
    ) -> Self {
        Self {
            module_ref,
            process_ref,
            host_requirements_ref,
            compilation_dialect,
            compiler_version: LASHLANG_COMPILER_VERSION,
            vm_abi_version: LASHLANG_VM_ABI_VERSION,
        }
    }
}

pub struct CompiledProcessCache {
    entries: VecDeque<CachedCompiledProcess>,
    hits: u64,
    misses: u64,
    evictions: u64,
    capacity: usize,
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
            entries: VecDeque::with_capacity(capacity),
            hits: 0,
            misses: 0,
            evictions: 0,
            capacity,
        }
    }

    pub fn get_or_compile(
        &mut self,
        artifact: &ModuleArtifact,
        process_ref: &ProcessRef,
        host_requirements_ref: &HostRequirementsRef,
    ) -> Result<Arc<CompiledProgram>, crate::RuntimeError> {
        let key = CompiledProcessCacheKey::new(
            artifact.module_ref.clone(),
            process_ref.clone(),
            host_requirements_ref.clone(),
            artifact.compilation_dialect,
        );
        if let Some(entry) = self.entries.back()
            && entry.key == key
        {
            self.hits += 1;
            return Ok(entry.compiled.clone());
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.hits += 1;
            let entry = self
                .entries
                .remove(index)
                .expect("cache index came from existing entry");
            let compiled = entry.compiled.clone();
            self.entries.push_back(entry);
            return Ok(compiled);
        }

        self.misses += 1;
        let compiled = Arc::new(compile_module_artifact_process(artifact, process_ref)?);
        if self.capacity == 0 {
            return Ok(compiled);
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            self.evictions += 1;
        }
        self.entries.push_back(CachedCompiledProcess {
            key,
            compiled: compiled.clone(),
        });
        Ok(compiled)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
    }

    pub fn stats(&self) -> CompiledProgramCacheStats {
        CompiledProgramCacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            entries: self.entries.len(),
            capacity: self.capacity,
        }
    }
}

impl Default for CompiledProcessCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Error)]
pub enum LinkedProgramCacheError {
    #[error(transparent)]
    Parse(#[from] crate::parser::ParseError),
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
    entries: VecDeque<CachedLinkedProgram>,
    hits: u64,
    misses: u64,
    evictions: u64,
    capacity: usize,
}

struct CachedLinkedProgram {
    source_hash: u64,
    source: Arc<str>,
    dialect: CompilationDialect,
    program: Arc<CompiledLinkedProgram>,
}

impl LinkedProgramCache {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_LINKED_PROGRAM_CACHE_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        prewarm();
        Self {
            entries: VecDeque::with_capacity(capacity),
            hits: 0,
            misses: 0,
            evictions: 0,
            capacity,
        }
    }

    /// Links and caches `source` as Lashlang, parsing it only when it is not
    /// already cached. A hit costs the lookup and nothing else: parsing on
    /// every call would make the cache pay a full parse per hit, which is the
    /// cost this cache exists to remove.
    pub fn get_or_compile(
        &mut self,
        source: &str,
        surface: impl Borrow<LashlangHostEnvironment>,
    ) -> Result<Arc<CompiledLinkedProgram>, LinkedProgramCacheError> {
        let surface = surface.borrow();
        if let Some(program) =
            self.cached_linked_program(source, surface, CompilationDialect::Lashlang)
        {
            return Ok(program);
        }

        let program = crate::parse(source)?;
        self.link_and_cache(source, program, surface, CompilationDialect::Lashlang)
            .map_err(LinkedProgramCacheError::Link)
    }

    /// Links and caches an already-parsed shared-AST program using the source
    /// dialect's bytecode semantics.
    ///
    /// A host whose dialect it parses itself should ask
    /// [`Self::cached_linked_program`] first, so that a hit does not pay for
    /// the parse this method's `program` argument required.
    pub fn get_or_compile_ast(
        &mut self,
        source: &str,
        program: Program,
        surface: impl Borrow<LashlangHostEnvironment>,
        dialect: CompilationDialect,
    ) -> Result<Arc<CompiledLinkedProgram>, LinkError> {
        let surface = surface.borrow();
        if let Some(program) = self.cached_linked_program(source, surface, dialect) {
            return Ok(program);
        }
        self.link_and_cache(source, program, surface, dialect)
    }

    /// The linked program already cached for this source, dialect and host
    /// surface, without parsing or linking anything.
    ///
    /// A hit is recorded and promoted exactly as it is on the compiling paths,
    /// so this is the lookup those paths use rather than a peek beside them.
    pub fn cached_linked_program(
        &mut self,
        source: &str,
        surface: impl Borrow<LashlangHostEnvironment>,
        dialect: CompilationDialect,
    ) -> Option<Arc<CompiledLinkedProgram>> {
        let source_hash = dialect_program_source_hash(source, dialect);
        let surface = surface.borrow();
        if let Some(entry) = self.entries.back()
            && linked_program_matches(entry, source_hash, source, surface, dialect)
        {
            self.hits += 1;
            return Some(entry.program.clone());
        }

        let index = self.entries.iter().position(|entry| {
            linked_program_matches(entry, source_hash, source, surface, dialect)
        })?;
        self.hits += 1;
        let entry = self
            .entries
            .remove(index)
            .expect("cache index came from existing entry");
        let program = entry.program.clone();
        self.entries.push_back(entry);
        Some(program)
    }

    fn link_and_cache(
        &mut self,
        source: &str,
        program: Program,
        surface: &LashlangHostEnvironment,
        dialect: CompilationDialect,
    ) -> Result<Arc<CompiledLinkedProgram>, LinkError> {
        let source_hash = dialect_program_source_hash(source, dialect);
        self.misses += 1;
        let linked = LinkedModule::link_with_dialect(program, surface, dialect)?;
        let compiled = Arc::new(compile_linked_with_dialect(&linked, dialect));
        let program = Arc::new(CompiledLinkedProgram { linked, compiled });
        if self.capacity == 0 {
            return Ok(program);
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            self.evictions += 1;
        }
        self.entries.push_back(CachedLinkedProgram {
            source_hash,
            source: Arc::<str>::from(source),
            dialect,
            program: program.clone(),
        });
        Ok(program)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
    }

    pub fn stats(&self) -> CompiledProgramCacheStats {
        CompiledProgramCacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            entries: self.entries.len(),
            capacity: self.capacity,
        }
    }
}

impl Default for LinkedProgramCache {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CompiledProgramCache {
    entries: VecDeque<CachedCompiledProgram>,
    hits: u64,
    misses: u64,
    evictions: u64,
    capacity: usize,
}

struct CachedCompiledProgram {
    source_hash: u64,
    source: Arc<str>,
    compiled: Arc<CompiledProgram>,
}

impl CompiledProgramCache {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_COMPILED_PROGRAM_CACHE_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        prewarm();
        Self {
            entries: VecDeque::with_capacity(capacity),
            hits: 0,
            misses: 0,
            evictions: 0,
            capacity,
        }
    }

    pub fn get_or_compile(
        &mut self,
        source: &str,
    ) -> Result<Arc<CompiledProgram>, crate::parser::ParseError> {
        let source_hash = program_source_hash(source);
        if let Some(entry) = self.entries.back()
            && program_source_matches(entry, source_hash, source)
        {
            self.hits += 1;
            return Ok(entry.compiled.clone());
        }

        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| program_source_matches(entry, source_hash, source))
        {
            self.hits += 1;
            let entry = self
                .entries
                .remove(index)
                .expect("cache index came from existing entry");
            let compiled = entry.compiled.clone();
            self.entries.push_back(entry);
            return Ok(compiled);
        }

        self.misses += 1;
        let program = crate::parse(source)?;
        let compiled = Arc::new(compile_program_internal(&program));
        if self.capacity == 0 {
            return Ok(compiled);
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            self.evictions += 1;
        }
        self.entries.push_back(CachedCompiledProgram {
            source_hash,
            source: Arc::<str>::from(source),
            compiled: compiled.clone(),
        });
        Ok(compiled)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
    }

    pub fn stats(&self) -> CompiledProgramCacheStats {
        CompiledProgramCacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            entries: self.entries.len(),
            capacity: self.capacity,
        }
    }
}

impl Default for CompiledProgramCache {
    fn default() -> Self {
        Self::new()
    }
}

fn program_source_matches(entry: &CachedCompiledProgram, source_hash: u64, source: &str) -> bool {
    source_matches(
        entry.source_hash,
        entry.source.as_ref(),
        source_hash,
        source,
    )
}

fn linked_program_matches(
    entry: &CachedLinkedProgram,
    source_hash: u64,
    source: &str,
    surface: &LashlangHostEnvironment,
    dialect: CompilationDialect,
) -> bool {
    source_matches(
        entry.source_hash,
        entry.source.as_ref(),
        source_hash,
        source,
    ) && entry.dialect == dialect
        && surface.satisfies(&entry.program.linked.artifact.host_requirements)
}

fn source_matches(cached_hash: u64, cached_source: &str, source_hash: u64, source: &str) -> bool {
    cached_hash == source_hash && cached_source == source
}

fn program_source_hash(source: &str) -> u64 {
    dialect_program_source_hash(source, CompilationDialect::Lashlang)
}

fn dialect_program_source_hash(source: &str, dialect: CompilationDialect) -> u64 {
    let mut hasher = FxHasher::default();
    SOURCE_CACHE_VERSION.hash(&mut hasher);
    match dialect {
        CompilationDialect::Lashlang => 0_u8,
        CompilationDialect::Typescript => 1_u8,
    }
    .hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}
