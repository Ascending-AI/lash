//! A compiled tier for library bodies (spike, FIG-5848).
//!
//! Each charged library code (a helper body or a closure inside one) is
//! lowered to one Cranelift function. The function runs the code's
//! statements with native control flow and calls into the machine's
//! runtime for every operation on values, so the operations, their order,
//! their charges and their reservations are the interpreter's.
//!
//! What it keeps canonical and what it rebuilds:
//!
//! - A frame's slots are always the frame's slots: the compiled code reads
//!   and writes them through the runtime.
//! - Loop and `try` entries of a frame's control stack are always on the
//!   stack; the compiled code pushes and pops them as the interpreter does.
//! - Block entries are not kept while compiled code runs. At every exit
//!   (a step boundary that slices, an action that pushes a frame or waits,
//!   a raise, a return, a statement it does not compile) the code names a
//!   *position*: the static list of block entries, with their `next`
//!   cursors, interleaved with the loop and `try` entries the stack holds.
//!   The runtime rebuilds the canonical stack from it, so what is saved at
//!   any safe point is what the interpreter would save there.
//!
//! The function is entered at the start of any statement it compiled: the
//! interpreter's `advance` looks up the innermost block and its cursor.

mod lower;

use std::collections::HashMap;
use std::time::Instant;

use crate::compile::{BlockId, CodeId, Lib, Tables};

/// One entry of a rebuilt control stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Level {
    Block {
        block: BlockId,
        next: u32,
    },
    /// The next loop or `try` entry the stack holds.
    Keep,
}

/// The function a code compiled to: `ctx` is the runtime's context.
pub(crate) type Entry = unsafe extern "C" fn(ctx: *mut u8) -> u32;

/// How a compiled function left.
pub(crate) const EXIT_STEP: u32 = 0;
pub(crate) const EXIT_DEOPT: u32 = 1;
pub(crate) const EXIT_OUTCOME: u32 = 2;

pub(crate) struct CompiledCode {
    pub(crate) entry: Entry,
    /// The entry index of each statement the function can start at, by
    /// its block (less `first_block`) and its index in the block; `NONE`
    /// where it cannot.
    pub(crate) entries: Vec<Box<[u32]>>,
    pub(crate) first_block: u32,
    pub(crate) positions: Vec<Box<[Level]>>,
    pub(crate) temps: u32,
    pub(crate) stats: CodeStats,
}

#[derive(Clone, Debug, Default)]
pub struct CodeStats {
    pub name: String,
    pub code_bytes: usize,
    pub compile_ns: u64,
    pub statements: u32,
    pub compiled_statements: u32,
}

/// Every compiled library code, by code id.
pub(crate) struct JitLibrary {
    pub(crate) codes: Vec<Option<CompiledCode>>,
    /// Owns the executable memory the entries point into.
    _module: ModuleHolder,
}

struct ModuleHolder(Option<cranelift_jit::JITModule>);

impl Drop for ModuleHolder {
    fn drop(&mut self) {
        if let Some(module) = self.0.take() {
            // SAFETY: the library that owned the entries is being dropped,
            // so no entry runs or is called again. cranelift-jit does not
            // free code memory on its own drop.
            #[expect(unsafe_code, reason = "spike: the entries die with the library")]
            unsafe {
                module.free_memory();
            }
        }
    }
}

// SAFETY: the module is only kept alive: its code memory is finalized,
// immutable and freed with the library.
#[expect(
    unsafe_code,
    reason = "spike: finalized JIT memory is immutable and shared read-only"
)]
unsafe impl Send for ModuleHolder {}
#[expect(
    unsafe_code,
    reason = "spike: finalized JIT memory is immutable and shared read-only"
)]
unsafe impl Sync for ModuleHolder {}

impl std::fmt::Debug for JitLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitLibrary")
            .field("compiled", &self.codes.iter().flatten().count())
            .finish_non_exhaustive()
    }
}

pub(crate) const NONE: u32 = u32::MAX;

impl CompiledCode {
    /// The entry of the statement `next` of `block`.
    #[inline]
    pub(crate) fn entry_at(&self, block: BlockId, next: usize) -> Option<u32> {
        let row = self
            .entries
            .get(block.0.checked_sub(self.first_block)? as usize)?;
        row.get(next).copied().filter(|entry| *entry != NONE)
    }
}

impl JitLibrary {
    pub(crate) fn code(&self, code: CodeId) -> Option<&CompiledCode> {
        self.codes.get(code.0 as usize).and_then(Option::as_ref)
    }

    pub(crate) fn stats(&self) -> Vec<CodeStats> {
        self.codes
            .iter()
            .flatten()
            .map(|code| code.stats.clone())
            .collect()
    }

    /// Compiles every charged code of a library's tables. `names` gives a
    /// code a name for the statistics.
    pub(crate) fn compile(
        tables: &Tables,
        libs: &[Lib],
        names: &HashMap<u32, String>,
        only: Option<&dyn Fn(&str) -> bool>,
    ) -> Self {
        let mut compiler = lower::Compiler::new();
        let mut pending = Vec::new();
        for (index, code) in tables.codes.iter().enumerate() {
            if !code.charged {
                continue;
            }
            let name = names
                .get(&(index as u32))
                .cloned()
                .unwrap_or_else(|| format!("code#{index}"));
            if let Some(only) = only
                && !only(&name)
            {
                continue;
            }
            let began = Instant::now();
            let Some(lowered) = compiler.lower(tables, libs, CodeId(index as u32), &name) else {
                continue;
            };
            let compile_ns = began.elapsed().as_nanos() as u64;
            pending.push((index, lowered, compile_ns, name));
        }
        let module = compiler.finish();
        let mut codes: Vec<Option<CompiledCode>> = Vec::new();
        codes.resize_with(tables.codes.len(), || None);
        for (index, lowered, compile_ns, name) in pending {
            let entry = module.entry(lowered.func);
            let first_block = lowered
                .entries
                .keys()
                .map(|(block, _)| *block)
                .min()
                .unwrap_or(0);
            let last_block = lowered
                .entries
                .keys()
                .map(|(block, _)| *block)
                .max()
                .unwrap_or(0);
            let mut entries: Vec<Vec<u32>> =
                vec![Vec::new(); (last_block + 1 - first_block) as usize];
            for ((block, next), index) in &lowered.entries {
                let row = &mut entries[(block - first_block) as usize];
                if row.len() <= *next as usize {
                    row.resize(*next as usize + 1, NONE);
                }
                row[*next as usize] = *index;
            }
            codes[index] = Some(CompiledCode {
                entry,
                entries: entries.into_iter().map(Vec::into_boxed_slice).collect(),
                first_block,
                positions: lowered.positions,
                temps: lowered.temps,
                stats: CodeStats {
                    name,
                    code_bytes: lowered.code_bytes,
                    compile_ns,
                    statements: lowered.statements,
                    compiled_statements: lowered.compiled_statements,
                },
            });
        }
        Self {
            codes,
            _module: ModuleHolder(module.into_module()),
        }
    }
}
