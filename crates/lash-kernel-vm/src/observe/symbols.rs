//! What the tokens an observation holds name: each code's function and
//! site, each library function's identity and name, and each statement's
//! site. Resolved after the fact, never while a run is observed.

use lash_kernel_value::names::FunctionId;
use lash_kernel_value::sites::{Site, Unit};

use crate::bytecode::program::{Program, UnitKind};

use super::work::{CodeCounts, NativeCounts};

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
}

impl LibKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Machine => "machine",
            Self::Body => "body",
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
    /// The codes of `program`, its own units and then the library's, each
    /// named by its unit and sited at its body. A spawn's stub is no code.
    /// The core counts no work and publishes no context, so its libraries
    /// and statements name nothing yet.
    pub(crate) fn of(program: &Program) -> Self {
        let units = program
            .units
            .iter()
            .chain(program.lib.units.iter().map(|unit| &**unit));
        let codes = units
            .filter_map(|unit| {
                let (kind, function) = match unit.kind {
                    UnitKind::Main => (CodeKind::Main, None),
                    UnitKind::Declared(function) => (CodeKind::Function, Some(function)),
                    UnitKind::Body(function) => (CodeKind::LibraryBody, Some(function)),
                    UnitKind::Closure(function) => (CodeKind::Closure, Some(function)),
                    UnitKind::Spawn => return None,
                };
                Some((kind, function, unit.name.to_string()))
            })
            .enumerate()
            .map(|(token, (kind, function, name))| CodeSymbol {
                token: token as u32,
                kind,
                name,
                function,
                site: Site::new(function.map_or(Unit::Main, Unit::Library), Vec::new()),
            })
            .collect();
        Self {
            codes,
            libs: Vec::new(),
            statements: Vec::new(),
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
