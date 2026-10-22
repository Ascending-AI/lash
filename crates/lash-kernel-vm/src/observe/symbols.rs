//! What the tokens an observation holds name: each code's function and
//! site, each library function's identity and name, and each statement's
//! site. Resolved after the fact, from the executable, never while a run
//! is observed.

use lash_vocabulary::names::FunctionId;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use super::work::{CodeCounts, Counting, NativeCounts, Owner};
use crate::compile::{CodeId, Executable, LibId, LibRun};

/// What kind of code a code token names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeKind {
    Main,
    /// A function the document declares.
    Function,
    /// The kernel-code body of a library function: a TypeScript or Python
    /// helper, or a kernel library body.
    LibraryBody,
    /// A closure, inside any of the others.
    Closure,
}

impl CodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Function => "function",
            Self::LibraryBody => "library_body",
            Self::Closure => "closure",
        }
    }
}

/// What kind of library function a library token names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibKind {
    /// A native implementation.
    Native,
    /// One of the machine's own functions.
    Machine,
    /// A function that runs its kernel-code body in a frame.
    Body,
    /// A function whose body's generated plan answers (FIG-6678).
    Plan,
}

impl LibKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Machine => "machine",
            Self::Body => "body",
            Self::Plan => "plan",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeSymbol {
    pub token: u32,
    pub kind: CodeKind,
    /// The function's name: `main`, a declared function's, a library
    /// function's registered name, or for a closure the name of the code
    /// it is in and its site there.
    pub name: String,
    /// The library function a library body or a closure inside one
    /// belongs to.
    pub function: Option<FunctionId>,
    /// The site of the body.
    pub site: Site,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibSymbol {
    pub token: u32,
    pub kind: LibKind,
    pub name: String,
    pub function: FunctionId,
}

/// Every token of one executable, resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestSymbols {
    pub codes: Vec<CodeSymbol>,
    pub libs: Vec<LibSymbol>,
    /// Each statement's site, by statement token.
    pub statements: Vec<Option<Site>>,
}

impl GuestSymbols {
    pub(crate) fn of(exe: &Executable) -> Self {
        let unit_name = |unit: &Unit| match unit {
            Unit::Main => "main".to_string(),
            Unit::Function(name) => name.to_string(),
            Unit::Library(function) => exe
                .library_name(function)
                .map_or_else(|| function.to_string(), ToString::to_string),
        };
        let codes = (0..exe.code_count() as u32)
            .map(|token| {
                let site = exe.code(CodeId(token)).site.clone();
                let function = match &site.unit {
                    Unit::Library(function) => Some(*function),
                    _ => None,
                };
                let (kind, name) = match (&site.unit, site.path.is_empty()) {
                    (_, false) => {
                        let path: Vec<String> = site.path.iter().map(u32::to_string).collect();
                        (
                            CodeKind::Closure,
                            format!("{}/closure@{}", unit_name(&site.unit), path.join(".")),
                        )
                    }
                    (Unit::Main, true) => (CodeKind::Main, unit_name(&site.unit)),
                    (Unit::Function(_), true) => (CodeKind::Function, unit_name(&site.unit)),
                    (Unit::Library(_), true) => (CodeKind::LibraryBody, unit_name(&site.unit)),
                };
                CodeSymbol {
                    token,
                    kind,
                    name,
                    function,
                    site,
                }
            })
            .collect();
        let libs = (0..exe.lib_count() as u32)
            .map(|token| {
                let lib = exe.lib(LibId(token));
                LibSymbol {
                    token,
                    kind: match lib.run {
                        LibRun::Native { .. } | LibRun::Declaration => LibKind::Native,
                        LibRun::Plan { .. } => LibKind::Plan,
                        LibRun::DeferredPlan { .. } => {
                            if exe.fallback(LibId(token)).is_some() {
                                LibKind::Plan
                            } else {
                                LibKind::Body
                            }
                        }
                        LibRun::Machine(_) => LibKind::Machine,
                        LibRun::Body(_) => LibKind::Body,
                    },
                    name: lib.header.name.to_string(),
                    function: lib.id,
                }
            })
            .collect();
        let mut statements = vec![None; exe.stmt_count()];
        for block in exe.blocks() {
            for (index, stmt) in block.stmts.iter().enumerate() {
                if let Some(slot) = statements.get_mut(stmt.0 as usize) {
                    *slot = Some(block.site.child(index as u32));
                }
            }
        }
        Self {
            codes,
            libs,
            statements,
        }
    }

    pub fn code(&self, token: u32) -> Option<&CodeSymbol> {
        self.codes.get(token as usize)
    }

    pub fn lib(&self, token: u32) -> Option<&LibSymbol> {
        self.libs.get(token as usize)
    }

    pub fn statement(&self, token: u32) -> Option<&Site> {
        self.statements.get(token as usize)?.as_ref()
    }
}

/// One code's counted work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeWork {
    pub symbol: CodeSymbol,
    pub counts: CodeCounts,
    /// The native calls it made, by library token.
    pub native_calls: Vec<(u32, u64)>,
}

/// One frameless library function's counted work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeWork {
    pub symbol: LibSymbol,
    pub counts: NativeCounts,
}

/// The work a run did since counting began, by function. Only functions
/// that did something have a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkReport {
    /// The charge since counting began: the machine's meter, read.
    pub charged: Option<u64>,
    /// The objects the heap took since counting began.
    pub heap_insertions: u64,
    /// Charge no function was running for. Every charge has a function, so
    /// a nonzero value is a counting fault.
    pub unattributed_charge: Option<u64>,
    /// Objects the machine made for no running function: a list `join`
    /// decided as its last member ends builds its result between frames.
    pub unattributed_insertions: u64,
    /// Native calls no frame made.
    pub unattributed_native_calls: Vec<(u32, u64)>,
    pub codes: Vec<CodeWork>,
    pub natives: Vec<NativeWork>,
}

impl WorkReport {
    pub(crate) fn of(
        counting: &Counting,
        symbols: &GuestSymbols,
        charged: u64,
        allocated: u64,
        metered: bool,
    ) -> Self {
        let mut counting = counting.clone();
        counting.flush(charged, allocated);
        counting.close_all();
        let calls_by = |owner: Owner| -> Vec<(u32, u64)> {
            counting
                .native_calls
                .range((owner, 0)..=(owner, u32::MAX))
                .map(|((_, lib), count)| (*lib, *count))
                .collect()
        };
        let codes = counting
            .codes
            .iter()
            .enumerate()
            .filter(|(_, counts)| **counts != CodeCounts::default())
            .filter_map(|(token, counts)| {
                Some(CodeWork {
                    symbol: symbols.code(token as u32)?.clone(),
                    counts: {
                        let mut counts = *counts;
                        counts.self_charge = metered.then(|| counts.self_charge.unwrap_or(0));
                        counts.inclusive_charge =
                            metered.then(|| counts.inclusive_charge.unwrap_or(0));
                        counts
                    },
                    native_calls: calls_by(Owner::Code(token as u32)),
                })
            })
            .collect();
        let natives = counting
            .natives
            .iter()
            .enumerate()
            .filter(|(_, counts)| **counts != NativeCounts::default())
            .filter_map(|(token, counts)| {
                Some(NativeWork {
                    symbol: symbols.lib(token as u32)?.clone(),
                    counts: {
                        let mut counts = *counts;
                        counts.self_charge = metered.then(|| counts.self_charge.unwrap_or(0));
                        counts.zero_charge_calls =
                            metered.then(|| counts.zero_charge_calls.unwrap_or(0));
                        counts
                    },
                })
            })
            .collect();
        Self {
            charged: metered.then(|| charged.saturating_sub(counting.began.0)),
            heap_insertions: allocated.saturating_sub(counting.began.1),
            unattributed_charge: metered.then_some(counting.unattributed_charge),
            unattributed_insertions: counting.unattributed_insertions,
            unattributed_native_calls: calls_by(Owner::Nobody),
            codes,
            natives,
        }
    }

    /// The run's charge less every self charge and the unattributed one:
    /// zero, when the counters saw every unit once.
    pub fn residual(&self) -> Option<i128> {
        let charged = self.charged?;
        let unattributed = self.unattributed_charge?;
        let selves: u64 = self
            .codes
            .iter()
            .map(|code| code.counts.self_charge.unwrap_or(0))
            .chain(
                self.natives
                    .iter()
                    .map(|native| native.counts.self_charge.unwrap_or(0)),
            )
            .sum();
        Some(i128::from(charged) - i128::from(selves) - i128::from(unattributed))
    }

    /// The objects the heap took less every function's and the
    /// unattributed ones: zero, when the counters saw every insertion once.
    pub fn insertion_residual(&self) -> i128 {
        let counted: u64 = self
            .codes
            .iter()
            .map(|code| code.counts.heap_insertions)
            .chain(
                self.natives
                    .iter()
                    .map(|native| native.counts.heap_insertions),
            )
            .sum();
        i128::from(self.heap_insertions)
            - i128::from(counted)
            - i128::from(self.unattributed_insertions)
    }
}
