//! The seeded generator of differential sessions (FIG-3608).
//!
//! A session is a few cells of ordinary model-shaped TypeScript drawn from the
//! dialect's accepted grammar: bindings at every scope (cell top level,
//! blocks, loop heads, function bodies, callbacks, catch clauses), shadowing,
//! closures, every loop form, the exotic built-ins, object and array
//! mutation, reassignment within a cell and through `globalThis` across
//! cells, rejected cells, and cells that throw or finish. Every construct
//! names the grammar row it draws from ([`Grammar`]): a census row, which
//! must be `accepted`, or the WHATWG URL surface the crate README accepts.
//!
//! Every cell is also valid JavaScript, so Node runs it unchanged: no
//! TypeScript-only syntax is drawn, because the Node oracle runs sources as
//! written.
//!
//! The generator is a pure function of its seed. It keeps a model of every
//! binding's scope and type, so it only ever emits a cell that is accepted
//! and terminates: loops are bounded by literals, recursion by small literal
//! arguments, and every read names a binding the model holds.
//!
//! It never draws a shape whose divergence is already named elsewhere, and
//! each such exclusion names what it excludes:
//!
//! * the registered deviations of the crate README it would otherwise hit:
//!   `cross-cell-redeclaration` (a top-level name is declared once per
//!   session), `global-object-aliases-lexical-bindings` (`globalThis` writes
//!   only a `var` or a `globalThis`-born name), `runtime-fault-brand` (no VM
//!   fault without an ECMA class; errors are thrown explicitly),
//!   `process-literal-is-a-process-value` (no top-level uncalled `async`
//!   arrow). `closure-boundary` is not
//!   excluded: a closure is drawn at the top level freely, the session names
//!   the rule, and no later cell reads it.
//! * the dialect's static refusals of JavaScript: an assignment to an earlier
//!   cell's binding (`TS_ASSIGN_CONST`; `globalThis` is the accepted form), a
//!   field an object literal's type lacks (`TS_LINK_ERROR`; a later cell may
//!   add one), block-level function declarations (Annex B, skipped by the
//!   census), an effect inside a builtin callback (`EffectInBuiltinCallback`),
//!   a reassigned `var`, parameter or catch binding (`TS_ASSIGN_CONST`).
//!   Closures read and assign the bindings they capture freely: a capture is
//!   exact (FIG-3707).
//! * what ECMA-262 leaves to the implementation, where Node's answer is not
//!   the only right one: `sort` with an inconsistent comparator (a `NaN`
//!   among the items of `right - left`), keys added to an object while
//!   `for...in` enumerates it.
//! * the open defects of the crate README, each until it is fixed:
//!   [`OPEN_DEFECT_EXCLUSIONS`].

use std::collections::BTreeSet;

/// The open defects the generator does not draw, each with the ticket that
/// fixes it. Each is pinned by a session-corpus row or a round-trip law row;
/// when one is fixed, its README entry goes, the test holding this list to
/// the README fails, and the exclusion is deleted here, which changes the
/// generated corpus deliberately.
pub(super) const OPEN_DEFECT_EXCLUSIONS: &[(&str, &str)] = &[];

/// Where a construct's grammar is accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Grammar {
    /// A row of the Test262 census (`kind`, `name`), which must be
    /// `accepted`.
    Census(&'static str, &'static str),
    /// A WHATWG URL class, whose accepted signatures the crate README lists
    /// and `tests/url_wpt.rs` pins; the census covers ECMA-262 only.
    Whatwg(&'static str),
}

const LANGUAGE: Grammar = Grammar::Census("directory", "language");
const BUILT_INS: Grammar = Grammar::Census("directory", "built-ins");
const LET: Grammar = Grammar::Census("feature", "let");
const CONST: Grammar = Grammar::Census("feature", "const");
const ARROW: Grammar = Grammar::Census("feature", "arrow-function");
const TEMPLATE: Grammar = Grammar::Census("feature", "template");
const FOR_OF: Grammar = Grammar::Census("feature", "for-of");
const FOR_IN_ORDER: Grammar = Grammar::Census("feature", "for-in-order");
const DESTRUCTURING_BINDING: Grammar = Grammar::Census("feature", "destructuring-binding");
const OBJECT_SPREAD: Grammar = Grammar::Census("feature", "object-spread");
const COALESCE: Grammar = Grammar::Census("feature", "coalesce-expression");
const OPTIONAL_CHAINING: Grammar = Grammar::Census("feature", "optional-chaining");
const EXPONENTIATION: Grammar = Grammar::Census("feature", "exponentiation");
const LOGICAL_ASSIGNMENT: Grammar = Grammar::Census("feature", "logical-assignment-operators");
const DEFAULT_PARAMETERS: Grammar = Grammar::Census("feature", "default-parameters");
const REST_PARAMETERS: Grammar = Grammar::Census("feature", "rest-parameters");
const GLOBAL_THIS: Grammar = Grammar::Census("feature", "globalThis");
const OPTIONAL_CATCH_BINDING: Grammar = Grammar::Census("feature", "optional-catch-binding");
const ERROR_CAUSE: Grammar = Grammar::Census("feature", "error-cause");
const COMPUTED_PROPERTY_NAMES: Grammar = Grammar::Census("feature", "computed-property-names");
const MAP: Grammar = Grammar::Census("feature", "Map");
const SET: Grammar = Grammar::Census("feature", "Set");
const SET_METHODS: Grammar = Grammar::Census("feature", "set-methods");
const CHANGE_ARRAY_BY_COPY: Grammar = Grammar::Census("feature", "change-array-by-copy");
const ARRAY_AT: Grammar = Grammar::Census("feature", "Array.prototype.at");
const ARRAY_INCLUDES: Grammar = Grammar::Census("feature", "Array.prototype.includes");
const ARRAY_FLAT: Grammar = Grammar::Census("feature", "Array.prototype.flat");
const ARRAY_FIND_FROM_LAST: Grammar = Grammar::Census("feature", "array-find-from-last");
const STRING_INCLUDES: Grammar = Grammar::Census("feature", "String.prototype.includes");
const STRING_REPLACE_ALL: Grammar = Grammar::Census("feature", "String.prototype.replaceAll");
const STRING_TRIMMING: Grammar = Grammar::Census("feature", "string-trimming");
const OBJECT_FROM_ENTRIES: Grammar = Grammar::Census("feature", "Object.fromEntries");
const OBJECT_HAS_OWN: Grammar = Grammar::Census("feature", "Object.hasOwn");
const REGEXP_NAMED_GROUPS: Grammar = Grammar::Census("feature", "regexp-named-groups");
const STABLE_SORT: Grammar = Grammar::Census("feature", "stable-array-sort");
const URL_CLASS: Grammar = Grammar::Whatwg("URL");
const URL_SEARCH_PARAMS: Grammar = Grammar::Whatwg("URLSearchParams");

/// Every grammar row the generator can draw. The bounded corpus must reach
/// each of them.
pub(super) const GRAMMAR: &[Grammar] = &[
    LANGUAGE,
    BUILT_INS,
    LET,
    CONST,
    ARROW,
    TEMPLATE,
    FOR_OF,
    FOR_IN_ORDER,
    DESTRUCTURING_BINDING,
    OBJECT_SPREAD,
    COALESCE,
    OPTIONAL_CHAINING,
    EXPONENTIATION,
    LOGICAL_ASSIGNMENT,
    DEFAULT_PARAMETERS,
    REST_PARAMETERS,
    GLOBAL_THIS,
    OPTIONAL_CATCH_BINDING,
    ERROR_CAUSE,
    COMPUTED_PROPERTY_NAMES,
    MAP,
    SET,
    SET_METHODS,
    CHANGE_ARRAY_BY_COPY,
    ARRAY_AT,
    ARRAY_INCLUDES,
    ARRAY_FLAT,
    ARRAY_FIND_FROM_LAST,
    STRING_INCLUDES,
    STRING_REPLACE_ALL,
    STRING_TRIMMING,
    OBJECT_FROM_ENTRIES,
    OBJECT_HAS_OWN,
    REGEXP_NAMED_GROUPS,
    STABLE_SORT,
    URL_CLASS,
    URL_SEARCH_PARAMS,
];

/// The census, read for its rejected rows: a generated rejected cell is a
/// census probe, which must reject with exactly the diagnostic its row names.
const CENSUS: &str = include_str!("../../../../../lash-typescript/tests/test262/census.tsv");

/// The census's rejected rows whose probe the dialect refuses statically, as
/// `(diagnostic, probe)`.
fn static_rejections() -> Vec<(&'static str, &'static str)> {
    CENSUS
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let columns = line.split('\t').collect::<Vec<_>>();
            (columns.len() == 5
                && columns[2] == "rejected"
                && !columns[4].starts_with("probe-exempt:"))
            .then(|| (columns[3], columns[4]))
        })
        .filter(|(diagnostic, probe)| {
            super::link_rejection(probe, BTreeSet::new(), BTreeSet::new())
                .is_some_and(|(code, _)| code == *diagnostic)
        })
        .collect()
}

/// A generated session: ordered cells and every binder name its cells
/// declare, all probed after every cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GeneratedSession {
    pub(super) probe: Vec<String>,
    pub(super) cells: Vec<GeneratedCell>,
    /// The grammar rows the session drew from.
    pub(super) grammar: BTreeSet<Grammar>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GeneratedCell {
    /// The cell's top-level statements, each possibly spanning lines. A
    /// minimizer removes whole statements.
    pub(super) statements: Vec<String>,
    /// The diagnostic a rejected cell (a census probe) must be refused with.
    pub(super) reject: Option<String>,
}

impl GeneratedCell {
    pub(super) fn source(&self) -> String {
        let mut source = self.statements.join("\n");
        source.push('\n');
        source
    }
}

/// Small, seekable and identical on every platform: the corpus must not
/// depend on a host's `rand`.
struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x3608_3608_3608_3608)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn range(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low + 1)
    }
}

/// The value model: what a binding holds, precisely enough to draw only
/// reads the linker accepts and operations that are defined.
#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Num,
    Str,
    Bool,
    /// A homogeneous array.
    Arr(Box<Ty>),
    /// An object literal's fields, in insertion order.
    Obj(Vec<(String, Ty)>),
    /// A `Map` from string keys.
    Map(Box<Ty>),
    /// A `Set` of numbers.
    Set,
    Date,
    Re,
    Url,
    Params,
    Err,
    /// A `RegExp` match or `null`.
    Match,
    /// A function of one number to a number. A recursive one is only ever
    /// called with a small literal.
    Fun {
        recursive: bool,
    },
}

impl Ty {
    fn reaches_function(&self) -> bool {
        match self {
            Ty::Fun { .. } => true,
            Ty::Arr(item) | Ty::Map(item) => item.reaches_function(),
            Ty::Obj(fields) => fields.iter().any(|(_, ty)| ty.reaches_function()),
            _ => false,
        }
    }

    /// Whether `JSON.stringify` keeps the value intact, so a JSON round trip
    /// is a deep copy of it.
    fn json_exact(&self) -> bool {
        match self {
            Ty::Num | Ty::Str | Ty::Bool => true,
            Ty::Arr(item) => item.json_exact(),
            Ty::Obj(fields) => fields.iter().all(|(_, ty)| ty.json_exact()),
            _ => false,
        }
    }

    fn is_compound(&self) -> bool {
        !matches!(self, Ty::Num | Ty::Str | Ty::Bool)
    }
}

/// How a binding was declared, which decides what may later write it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decl {
    Let,
    Const,
    Var,
    /// A parameter, a loop head or a catch binding.
    Local,
    /// A session global from an earlier cell: readable and mutable in place,
    /// never assigned directly (`TS_ASSIGN_CONST`).
    Session,
    /// A session global a later cell may write through `globalThis`: an
    /// earlier cell's `var`, or a name `globalThis` created.
    SessionVar,
}

#[derive(Clone, Debug)]
struct Binding {
    name: String,
    ty: Ty,
    decl: Decl,
    /// Nothing may assign it any more: a closure captured it, or it is a
    /// `for...of` iterable whose body is running.
    frozen: bool,
    /// Hidden from every read: the iterable of the enclosing `for...of`.
    hidden: bool,
    /// The value's identity: bindings that alias one object share it.
    identity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeKind {
    CellTop,
    Block,
    /// A function or arrow body: outer bindings are captures.
    Function,
}

struct Scope {
    kind: ScopeKind,
    bindings: Vec<Binding>,
    /// Where the scope's text starts in the statement being written: a
    /// declaration in it must not take a name its earlier text reads, which
    /// would be a read in the temporal dead zone.
    start: usize,
}

/// Names a session draws from. Small on purpose, so block bindings shadow
/// outer ones often.
const NAMES: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "omega", "kappa", "sigma", "theta", "lambda", "zeta",
    "iota", "rho", "tau", "phi", "psi", "chi",
];
const KEYS: &[&str] = &["a", "b", "c", "d", "e", "f", "g", "h"];
const WORDS: &[&str] = &["x", "hi", "a,b", "Lash", "  pad ", "ab", "b.c", "zz"];

const MAX_DEPTH: usize = 2;

/// Generates the session of `seed`.
pub(super) fn generate(seed: u64) -> GeneratedSession {
    Generator::new(seed).session()
}

struct Generator {
    prng: Prng,
    scopes: Vec<Scope>,
    /// Session globals surviving from earlier cells.
    globals: Vec<Binding>,
    /// Every name any scope of the session declared, probed after each cell.
    binders: BTreeSet<String>,
    /// Names declared at a cell's top level by any cell, which no later
    /// top-level declaration may reuse (`cross-cell-redeclaration`), and
    /// names any `var` or `globalThis` write created, which no declaration
    /// anywhere may reuse (a `var` hoists across blocks).
    top_level_names: BTreeSet<String>,
    var_names: BTreeSet<String>,
    lines: Vec<String>,
    indent: usize,
    grammar: BTreeSet<Grammar>,
    /// A counter for the fresh names loop counters and temporaries take.
    fresh: usize,
    /// Nesting depth of effect-free code: a builtin callback, or a function
    /// body, which a callback may call. No effect may run inside one.
    callbacks: usize,
    /// Nesting depth of code that may not run (an `if` arm, a `switch` case,
    /// a `for...of` or `for...in` body, a `try` or `catch` block): a `var`
    /// declared there hoists but may stay `undefined`, so none is.
    uncertain: usize,
    /// The next fresh value identity, and the identity the next declaration
    /// takes when its value is an alias of another binding's.
    next_identity: usize,
    alias_of: Option<usize>,
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            prng: Prng::new(seed),
            scopes: Vec::new(),
            globals: Vec::new(),
            binders: BTreeSet::new(),
            top_level_names: BTreeSet::new(),
            var_names: BTreeSet::new(),
            lines: Vec::new(),
            indent: 0,
            grammar: BTreeSet::new(),
            fresh: 0,
            callbacks: 0,
            uncertain: 0,
            next_identity: 1,
            alias_of: None,
        }
    }

    fn session(mut self) -> GeneratedSession {
        self.uses(LANGUAGE);
        let cell_count = self.prng.range(2, 4);
        let mut cells = Vec::with_capacity(cell_count);
        for index in 0..cell_count {
            if index > 0 && self.prng.chance(8) {
                cells.push(self.rejected_cell());
                continue;
            }
            cells.push(self.cell());
        }
        GeneratedSession {
            probe: self.binders.into_iter().collect(),
            cells,
            grammar: self.grammar,
        }
    }

    fn uses(&mut self, grammar: Grammar) {
        self.grammar.insert(grammar);
    }

    // --- cells ---------------------------------------------------------------

    /// A census probe: a cell the dialect refuses statically by the
    /// diagnostic its row names, which must leave the session exactly as it
    /// was. A row refused only at run time (`TS_PENDING_TOOL`) is not a
    /// static rejection, so it is not drawn.
    fn rejected_cell(&mut self) -> GeneratedCell {
        let rows = static_rejections();
        let (diagnostic, probe) = *self.prng.pick(&rows);
        GeneratedCell {
            statements: vec![probe.to_string()],
            reject: Some(diagnostic.to_string()),
        }
    }

    fn cell(&mut self) -> GeneratedCell {
        self.scopes.push(Scope {
            kind: ScopeKind::CellTop,
            bindings: Vec::new(),
            start: 0,
        });
        let mut statements = Vec::new();
        let count = self.prng.range(2, 6);
        for _ in 0..count {
            self.statement(0);
            statements.push(self.take_lines());
        }
        match self.prng.below(20) {
            0 => {
                let class = *self.prng.pick(&["Error", "RangeError", "TypeError"]);
                let word = self.word();
                self.emit(format!("throw new {class}({word});"));
                statements.push(self.take_lines());
            }
            1 => {
                let (value, _) = self.primitive(0);
                self.uses(BUILT_INS);
                self.emit(format!("finish({value});"));
                statements.push(self.take_lines());
            }
            _ => {}
        }
        let top = self.scopes.pop().expect("the cell's top scope");
        for mut binding in top.bindings {
            // What survives the cell: a binding whose value reaches a
            // function is dropped (`closure-boundary`), so no later cell
            // reads it.
            if binding.ty.reaches_function() {
                continue;
            }
            binding.decl = match binding.decl {
                Decl::Var | Decl::SessionVar => Decl::SessionVar,
                _ => Decl::Session,
            };
            binding.frozen = false;
            self.globals.retain(|global| global.name != binding.name);
            self.globals.push(binding);
        }
        GeneratedCell {
            statements,
            reject: None,
        }
    }

    fn take_lines(&mut self) -> String {
        std::mem::take(&mut self.lines).join("\n")
    }

    fn emit(&mut self, line: String) {
        self.lines
            .push(format!("{}{line}", "  ".repeat(self.indent)));
    }

    // --- scopes --------------------------------------------------------------

    fn push_scope(&mut self, kind: ScopeKind) {
        self.scopes.push(Scope {
            kind,
            bindings: Vec::new(),
            start: self.lines.len(),
        });
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn at_cell_top(&self) -> bool {
        self.scopes.len() == 1
    }

    /// Every binding a read may name: the innermost of each name, the session
    /// globals beneath the cell.
    fn visible(&self) -> Vec<Binding> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for scope in self.scopes.iter().rev() {
            for binding in scope.bindings.iter().rev() {
                if !seen.insert(binding.name.clone()) {
                    continue;
                }
                if binding.hidden {
                    continue;
                }
                out.push(binding.clone());
            }
        }
        for binding in self.globals.iter().rev() {
            if !seen.insert(binding.name.clone()) || binding.hidden {
                continue;
            }
            out.push(binding.clone());
        }
        out
    }

    fn visible_where(&self, keep: impl Fn(&Binding) -> bool) -> Vec<Binding> {
        self.visible().into_iter().filter(|b| keep(b)).collect()
    }

    fn binding_mut(&mut self, name: &str) -> Option<&mut Binding> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.bindings.iter_mut().rev().find(|b| b.name == name) {
                return Some(binding);
            }
        }
        self.globals.iter_mut().rev().find(|b| b.name == name)
    }

    fn declare(&mut self, name: &str, ty: Ty, decl: Decl) {
        self.binders.insert(name.to_string());
        let identity = self.alias_of.take().unwrap_or_else(|| {
            self.next_identity += 1;
            self.next_identity
        });
        let binding = Binding {
            name: name.to_string(),
            ty,
            decl,
            frozen: false,
            hidden: false,
            identity,
        };
        if decl == Decl::Var {
            // A `var` belongs to the nearest function scope or the cell.
            let index = self
                .scopes
                .iter()
                .rposition(|scope| scope.kind != ScopeKind::Block)
                .expect("a cell or function scope");
            self.scopes[index].bindings.push(binding);
        } else {
            self.scopes
                .last_mut()
                .expect("a scope")
                .bindings
                .push(binding);
        }
    }

    fn var_hoists_to_top(&self) -> bool {
        !self
            .scopes
            .iter()
            .any(|scope| scope.kind == ScopeKind::Function)
    }

    /// A name for a new declaration. At the cell top, or for a `var`, it is
    /// new to the session; in a block it may shadow a visible binding, but
    /// never one its scope's earlier text or its own initializer (`avoid`)
    /// reads: that read would be in the temporal dead zone.
    fn new_name(&mut self, decl: Decl, avoid: &str) -> Option<String> {
        let hoisting = decl == Decl::Var;
        let top = self.at_cell_top() || hoisting && self.var_hoists_to_top();
        let in_scope = self
            .scopes
            .last()
            .map(|scope| {
                scope
                    .bindings
                    .iter()
                    .map(|b| b.name.clone())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let every_declared = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.iter().map(|b| b.name.clone()))
            .collect::<BTreeSet<_>>();
        let start = self.scopes.last().map_or(0, |scope| scope.start);
        let mut read = self.lines[start.min(self.lines.len())..].join("\n");
        read.push('\n');
        read.push_str(avoid);
        let candidates = NAMES
            .iter()
            .map(|name| (*name).to_string())
            .filter(|name| {
                if self.var_names.contains(name) || in_scope.contains(name) {
                    return false;
                }
                if top || hoisting {
                    // A top-level or hoisted name is new to the session and
                    // to every scope of this cell.
                    !self.top_level_names.contains(name)
                        && !self.binders.contains(name)
                        && !every_declared.contains(name)
                } else {
                    !mentions(&read, name)
                }
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let name = self.prng.pick(&candidates).clone();
        if top {
            self.top_level_names.insert(name.clone());
        }
        if hoisting {
            self.var_names.insert(name.clone());
        }
        Some(name)
    }

    /// A name no scope can see: for a function declaration, whose hoisted
    /// name its own body would otherwise read in place of an outer one.
    fn unshadowing_name(&mut self) -> Option<String> {
        let visible = self
            .visible()
            .into_iter()
            .map(|binding| binding.name)
            .collect::<Vec<_>>()
            .join(" ");
        self.new_name(Decl::Const, &visible)
    }

    fn fresh_name(&mut self, stem: &str) -> String {
        self.fresh += 1;
        let name = format!("{stem}{}", self.fresh);
        self.top_level_names.insert(name.clone());
        name
    }

    // --- statements ----------------------------------------------------------

    fn statement(&mut self, depth: usize) {
        let choice = self.prng.below(if depth >= MAX_DEPTH { 12 } else { 23 });
        match choice {
            0..=3 => self.declaration(),
            4..=5 => self.print(),
            6..=8 => self.mutation(),
            9 => self.reassignment(),
            10 => self.closure_declaration(),
            11 => self.local_exotic(depth),
            12 => self.block(depth),
            13 => self.uncertainly(|this| this.if_statement(depth)),
            14 => self.classic_for(depth),
            15 => self.uncertainly(|this| this.for_of(depth)),
            16 => self.uncertainly(|this| this.for_in(depth)),
            17 => self.while_loop(depth),
            18 => self.uncertainly(|this| this.switch(depth)),
            19 => self.uncertainly(|this| this.try_statement(depth)),
            20 => self.function_declaration(depth),
            21 => self.collect(),
            _ => self.destructuring(),
        }
    }

    /// Draws a statement whose blocks may not run.
    fn uncertainly(&mut self, statement: impl FnOnce(&mut Self)) {
        self.uncertain += 1;
        statement(self);
        self.uncertain -= 1;
    }

    fn body(&mut self, depth: usize, count: usize) {
        for _ in 0..count {
            self.statement(depth + 1);
        }
    }

    fn declaration(&mut self) {
        let decl = match self.prng.below(10) {
            0..=3 => Decl::Const,
            4..=7 => Decl::Let,
            _ if self.uncertain == 0 => Decl::Var,
            _ => Decl::Let,
        };
        let (value, ty) = self.any_value(0);
        let Some(name) = self.new_name(decl, &value) else {
            self.alias_of = None;
            return self.print();
        };
        let keyword = match decl {
            Decl::Const => {
                self.uses(CONST);
                "const"
            }
            Decl::Let => {
                self.uses(LET);
                "let"
            }
            _ => "var",
        };
        self.emit(format!("{keyword} {name} = {value};"));
        self.declare(&name, ty, decl);
    }

    fn print(&mut self) {
        if self.callbacks > 0 {
            return;
        }
        let count = self.prng.range(1, 3);
        let mut parts = Vec::new();
        for _ in 0..count {
            parts.push(self.printable());
        }
        self.uses(BUILT_INS);
        let method = *self.prng.pick(&["log", "log", "log", "info", "warn"]);
        self.emit(format!("console.{method}({});", parts.join(", ")));
    }

    /// Something whose printed form both engines agree on: a primitive, a
    /// plain object or array, a `RegExp`, `URL`, `URLSearchParams` or `Error`
    /// through `ToString`, a `Date` through `toISOString`.
    fn printable(&mut self) -> String {
        let visible = self.visible_where(|b| !b.ty.reaches_function());
        if !visible.is_empty() && self.prng.chance(55) {
            let binding = self.prng.pick(&visible).clone();
            return match binding.ty {
                Ty::Date => format!("{}.toISOString()", binding.name),
                Ty::Map(_) => {
                    self.uses(MAP);
                    format!("[...{}.entries()]", binding.name)
                }
                Ty::Set => {
                    self.uses(SET);
                    format!("[...{}]", binding.name)
                }
                Ty::Match => format!("{} === null ? 'none' : {}[0]", binding.name, binding.name),
                Ty::Obj(_) if self.prng.chance(30) => {
                    // A spread copy with a field the original lacks, printed
                    // whole.
                    self.uses(OBJECT_SPREAD);
                    format!("{{ ...{}, z: 0 }}", binding.name)
                }
                _ => binding.name,
            };
        }
        let ty = self.prng.pick(&[Ty::Num, Ty::Str, Ty::Bool]).clone();
        self.expr(&ty, 0)
    }

    /// An in-place change to a visible object, array or exotic.
    fn mutation(&mut self) {
        let targets = self.visible_where(|b| {
            matches!(
                b.ty,
                Ty::Arr(_) | Ty::Obj(_) | Ty::Map(_) | Ty::Set | Ty::Url | Ty::Params | Ty::Re
            )
        });
        if targets.is_empty() {
            return self.declaration();
        }
        let target = self.prng.pick(&targets).clone();
        let name = target.name.clone();
        match target.ty.clone() {
            Ty::Arr(item) => {
                let value = self.value_for(&item);
                match self.prng.below(8) {
                    0 => self.emit(format!("{name}.push({value});")),
                    1 => {
                        let more = self.value_for(&item);
                        self.emit(format!("{name}.push(...[{value}, {more}]);"));
                    }
                    2 => self.emit(format!("{name}.unshift({value});")),
                    3 => self.emit(format!("{name}[0] = {value};")),
                    4 => self.emit(format!("{name}.pop();")),
                    5 => self.emit(format!("{name}.splice(0, 1);")),
                    6 => {
                        // A consistent comparator: `right - left` is not one
                        // once a `NaN` is among the items, and an
                        // inconsistent comparator's order is
                        // implementation-defined.
                        self.uses(STABLE_SORT);
                        if self.prng.chance(50) {
                            self.emit(format!("{name}.sort();"));
                        } else {
                            self.emit(format!(
                                "{name}.sort((left, right) => (String(left) < String(right) ? 1 : String(left) > String(right) ? -1 : 0));"
                            ));
                            self.binders.insert("left".to_string());
                            self.binders.insert("right".to_string());
                        }
                    }
                    _ => self.emit(format!("{name}.reverse();")),
                }
            }
            Ty::Obj(fields) => {
                // A later cell may add a field to an earlier cell's `let` or
                // `const`, where it runs for certain. (After a `globalThis`
                // write in the same cell the linker types a `var` by the
                // written literal, which a new field is not in.)
                let added_later = target.decl == Decl::Session && self.uncertain == 0;
                if added_later && self.prng.chance(35) {
                    let keys = KEYS
                        .iter()
                        .filter(|key| fields.iter().all(|(field, _)| field != *key))
                        .copied()
                        .collect::<Vec<_>>();
                    if !keys.is_empty() {
                        let key = *self.prng.pick(&keys);
                        let ty = self.prng.pick(&[Ty::Num, Ty::Str, Ty::Bool]).clone();
                        let value = self.expr(&ty, 1);
                        self.emit(format!("{name}.{key} = {value};"));
                        let mut fields = fields;
                        fields.push((key.to_string(), ty));
                        if let Some(binding) = self.binding_mut(&name) {
                            binding.ty = Ty::Obj(fields);
                        }
                        return;
                    }
                }
                if fields.is_empty() {
                    return self.print();
                }
                let (key, ty) = self.prng.pick(&fields).clone();
                match ty {
                    Ty::Num if self.prng.chance(50) => {
                        let (op, value) = self.compound_operand();
                        self.emit(format!("{name}.{key} {op}= {value};"));
                    }
                    Ty::Num | Ty::Str | Ty::Bool => {
                        let value = self.expr(&ty, 1);
                        self.emit(format!("{name}.{key} = {value};"));
                    }
                    Ty::Arr(item) => {
                        let value = self.value_for(&item);
                        self.emit(format!("{name}.{key}.push({value});"));
                    }
                    _ => {
                        let value = self.value_for(&ty);
                        self.emit(format!("{name}.{key} = {value};"));
                    }
                }
            }
            Ty::Map(item) => {
                self.uses(MAP);
                let key = self.word();
                if self.prng.chance(70) {
                    let value = self.value_for(&item);
                    self.emit(format!("{name}.set({key}, {value});"));
                } else {
                    self.emit(format!("{name}.delete({key});"));
                }
            }
            Ty::Set => {
                self.uses(SET);
                let value = self.number_literal();
                let method = if self.prng.chance(70) {
                    "add"
                } else {
                    "delete"
                };
                self.emit(format!("{name}.{method}({value});"));
            }
            Ty::Url => {
                self.uses(URL_CLASS);
                self.uses(URL_SEARCH_PARAMS);
                let word = self.word();
                match self.prng.below(3) {
                    0 => self.emit(format!("{name}.pathname = '/p' + {word}.length;")),
                    1 => self.emit(format!("{name}.searchParams.append('k', {word});")),
                    _ => self.emit(format!("{name}.hash = 'h';")),
                }
            }
            Ty::Params => {
                self.uses(URL_SEARCH_PARAMS);
                let word = self.word();
                match self.prng.below(3) {
                    0 => self.emit(format!("{name}.append('k', {word});")),
                    1 => self.emit(format!("{name}.set('a', {word});")),
                    _ => self.emit(format!("{name}.sort();")),
                }
            }
            _ => {
                self.uses(BUILT_INS);
                self.emit(format!("{name}.lastIndex = 0;"));
            }
        }
    }

    /// `source.forEach((value) => { sink.push(value); })`: a collection's
    /// own `forEach`, or an array's, into an array its callback captures.
    fn collect(&mut self) {
        if self.callbacks > 0 {
            return self.print();
        }
        let sources =
            self.visible_where(|b| matches!(b.ty, Ty::Arr(_) | Ty::Map(_) | Ty::Set | Ty::Params));
        if sources.is_empty() {
            return self.mutation();
        }
        let source = self.prng.pick(&sources).clone();
        let item = match &source.ty {
            Ty::Arr(item) | Ty::Map(item) => (**item).clone(),
            Ty::Set => Ty::Num,
            _ => Ty::Str,
        };
        // The sink is not the source: an array's `forEach` would not visit
        // what it appends.
        let sinks = self.visible_where(|b| {
            b.ty == Ty::Arr(Box::new(item.clone()))
                && b.name != source.name
                && matches!(
                    b.decl,
                    Decl::Const | Decl::Let | Decl::Local | Decl::Session
                )
        });
        if sinks.is_empty() {
            return self.mutation();
        }
        let sink = self.prng.pick(&sinks).name.clone();
        match source.ty {
            Ty::Map(_) => self.uses(MAP),
            Ty::Set => self.uses(SET),
            Ty::Params => self.uses(URL_SEARCH_PARAMS),
            _ => {}
        }
        self.uses(ARROW);
        self.binders.insert("value".to_string());
        self.emit(format!(
            "{}.forEach((value) => {{ {sink}.push(value); }});",
            source.name
        ));
    }

    fn compound_operand(&mut self) -> (&'static str, String) {
        match self.prng.below(4) {
            0 => ("+", self.number_literal()),
            1 => ("-", self.number_literal()),
            2 => ("*", format!("{}", self.prng.range(1, 3))),
            _ => {
                self.uses(EXPONENTIATION);
                ("**", "1".to_string())
            }
        }
    }

    /// A reassignment: of a `let` declared in this cell, from its own frame
    /// or a closure over it, or across cells through `globalThis`.
    fn reassignment(&mut self) {
        let in_function = self
            .scopes
            .iter()
            .any(|scope| scope.kind == ScopeKind::Function);
        let mut locals = Vec::new();
        for scope in &self.scopes {
            for binding in &scope.bindings {
                // A `var`, like a parameter or catch binding, is not
                // assignable after its declaration (`TS_ASSIGN_CONST`).
                if binding.decl == Decl::Let && !binding.frozen && !binding.hidden {
                    locals.push(binding.clone());
                }
            }
        }
        // Only the innermost binding of a name is assignable by it.
        let visible = self.visible();
        locals.retain(|local| {
            visible
                .iter()
                .any(|b| b.name == local.name && b.decl == local.decl && b.ty == local.ty)
        });
        let session_vars = if in_function || self.callbacks > 0 {
            Vec::new()
        } else {
            self.visible_where(|b| b.decl == Decl::SessionVar && !b.ty.reaches_function())
        };
        if !session_vars.is_empty() && (locals.is_empty() || self.prng.chance(40)) {
            let target = self.prng.pick(&session_vars).clone();
            let value = self.value_for(&target.ty);
            self.uses(GLOBAL_THIS);
            self.emit(format!("globalThis.{} = {value};", target.name));
            return;
        }
        if locals.is_empty() {
            if !in_function && self.callbacks == 0 && self.at_cell_top() && self.prng.chance(40) {
                // `globalThis` creates a session global a bare name reads.
                let name = self.fresh_name("shared");
                let (value, ty) = self.any_value(1);
                self.uses(GLOBAL_THIS);
                self.emit(format!("globalThis.{name} = {value};"));
                self.var_names.insert(name.clone());
                self.declare(&name, ty, Decl::Var);
                if let Some(binding) = self.binding_mut(&name) {
                    // Only `globalThis` writes it.
                    binding.frozen = true;
                }
                return;
            }
            return self.print();
        }
        let target = self.prng.pick(&locals).clone();
        let name = target.name.clone();
        match &target.ty {
            Ty::Num => match self.prng.below(4) {
                0 => self.emit(format!("{name}++;")),
                1 => {
                    let (op, value) = self.compound_operand();
                    self.emit(format!("{name} {op}= {value};"));
                }
                2 => {
                    self.uses(LOGICAL_ASSIGNMENT);
                    let value = self.expr(&Ty::Num, 1);
                    self.emit(format!("{name} ||= {value};"));
                }
                _ => {
                    let value = self.expr(&Ty::Num, 1);
                    self.emit(format!("{name} = {value};"));
                }
            },
            Ty::Str => {
                if self.prng.chance(50) {
                    let word = self.word();
                    self.emit(format!("{name} += {word};"));
                } else {
                    self.uses(LOGICAL_ASSIGNMENT);
                    let value = self.expr(&Ty::Str, 1);
                    self.emit(format!("{name} &&= {value};"));
                }
            }
            ty => {
                let value = self.value_for(ty);
                self.emit(format!("{name} = {value};"));
            }
        }
    }

    /// An arrow bound to a name: at the cell top it is a top-level closure,
    /// dropped at the end of the cell (`closure-boundary`).
    fn closure_declaration(&mut self) {
        let arrow = self.arrow();
        let Some(name) = self.new_name(Decl::Const, &arrow) else {
            return self.print();
        };
        self.uses(CONST);
        self.emit(format!("const {name} = {arrow};"));
        self.declare(&name, Ty::Fun { recursive: false }, Decl::Const);
        // Use it at once: a closure is only meaningful in its own cell.
        if self.callbacks == 0 {
            let argument = self.expr(&Ty::Num, 1);
            self.uses(BUILT_INS);
            self.emit(format!("console.log({name}({argument}));"));
        }
    }

    /// An arrow `(number) => number` over the visible captures.
    fn arrow(&mut self) -> String {
        self.uses(ARROW);
        let param = *self.prng.pick(&["n", "value", "item"]);
        self.binders.insert(param.to_string());
        self.push_scope(ScopeKind::Function);
        self.declare(param, Ty::Num, Decl::Local);
        let text = match self.prng.below(4) {
            0 => {
                self.uses(DEFAULT_PARAMETERS);
                let body = self.expr(&Ty::Num, 1);
                format!("({param}, step = 2) => {body} + step")
            }
            1 => {
                self.uses(REST_PARAMETERS);
                self.binders.insert("rest".to_string());
                let body = self.expr(&Ty::Num, 1);
                format!("({param}, ...rest) => {body} + rest.length")
            }
            2 => {
                // A block body with a local of its own.
                let local = *self.prng.pick(&["local", "scaled"]);
                self.binders.insert(local.to_string());
                let init = self.expr(&Ty::Num, 1);
                self.declare(local, Ty::Num, Decl::Const);
                let body = self.expr(&Ty::Num, 1);
                format!("({param}) => {{ const {local} = {init}; return {body}; }}")
            }
            _ => {
                let body = self.expr(&Ty::Num, 1);
                format!("({param}) => {body}")
            }
        };
        self.pop_scope();
        text
    }

    /// A function declaration at a cell's or a function's top level (never in
    /// a block: that is Annex B's hoisting, which the census skips).
    fn function_declaration(&mut self, depth: usize) {
        let at_function_top = self
            .scopes
            .last()
            .is_some_and(|scope| scope.kind != ScopeKind::Block);
        if !at_function_top || self.callbacks > 0 {
            return self.block(depth);
        }
        let Some(name) = self.unshadowing_name() else {
            return self.print();
        };
        let recursive = self.prng.chance(30);
        let uncertain = std::mem::take(&mut self.uncertain);
        self.binders.insert(name.clone());
        self.emit(format!("function {name}(count) {{"));
        self.binders.insert("count".to_string());
        self.indent += 1;
        self.push_scope(ScopeKind::Function);
        self.declare("count", Ty::Num, Decl::Local);
        // A body is effect-free, so a builtin callback may call it.
        self.callbacks += 1;
        if recursive {
            self.emit(format!(
                "return count <= 1 ? 1 : count + {name}(count - 1);"
            ));
        } else {
            let statements = self.prng.range(0, 2);
            for _ in 0..statements {
                self.statement(MAX_DEPTH);
            }
            let result = self.expr(&Ty::Num, 1);
            self.emit(format!("return {result};"));
        }
        self.callbacks -= 1;
        self.uncertain = uncertain;
        self.pop_scope();
        self.indent -= 1;
        self.emit("}".to_string());
        // Declared after its body: a body never calls its own name except
        // in the recursive form above.
        let decl_scope = self.scopes.len() - 1;
        self.scopes[decl_scope].bindings.push(Binding {
            name: name.clone(),
            ty: Ty::Fun { recursive },
            decl: Decl::Const,
            frozen: true,
            hidden: false,
            identity: 0,
        });
        if self.callbacks == 0 {
            let argument = if recursive {
                self.prng.range(1, 6).to_string()
            } else {
                self.expr(&Ty::Num, 1)
            };
            self.emit(format!("console.log({name}({argument}));"));
        }
    }

    /// A block-scoped exotic: a `Map`, `Set`, `Date`, `RegExp`, `URL`,
    /// `URLSearchParams`, `Error` or match, declared and used where it
    /// cannot reach the session.
    fn local_exotic(&mut self, depth: usize) {
        if self.at_cell_top() || self.callbacks > 0 {
            self.emit("{".to_string());
            self.indent += 1;
            self.push_scope(ScopeKind::Block);
            self.local_exotic_here();
            let count = self.prng.range(0, 2);
            self.body(depth, count);
            self.pop_scope();
            self.indent -= 1;
            self.emit("}".to_string());
            return;
        }
        self.local_exotic_here();
    }

    fn local_exotic_here(&mut self) {
        let ty = self
            .prng
            .pick(&[
                Ty::Map(Box::new(Ty::Num)),
                Ty::Set,
                Ty::Date,
                Ty::Re,
                Ty::Url,
                Ty::Params,
                Ty::Err,
                Ty::Match,
            ])
            .clone();
        let value = self.exotic(&ty);
        let Some(name) = self.new_name(Decl::Const, &value) else {
            return;
        };
        self.uses(CONST);
        self.emit(format!("const {name} = {value};"));
        self.declare(&name, ty.clone(), Decl::Const);
        if self.callbacks == 0 {
            let reads = self.exotic_reads(&name, &ty);
            self.uses(BUILT_INS);
            self.emit(format!("console.log({reads});"));
        }
    }

    fn block(&mut self, depth: usize) {
        self.emit("{".to_string());
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        let count = self.prng.range(1, 3);
        self.body(depth, count);
        self.pop_scope();
        self.indent -= 1;
        self.emit("}".to_string());
    }

    fn braced(&mut self, head: String, depth: usize, count: usize) {
        self.emit(format!("{head} {{"));
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        self.body(depth, count);
        self.pop_scope();
        self.indent -= 1;
    }

    fn if_statement(&mut self, depth: usize) {
        let condition = self.expr(&Ty::Bool, 1);
        let count = self.prng.range(1, 2);
        self.braced(format!("if ({condition})"), depth, count);
        if self.prng.chance(50) {
            self.emit("} else {".to_string());
            self.indent += 1;
            self.push_scope(ScopeKind::Block);
            let count = self.prng.range(1, 2);
            self.body(depth, count);
            self.pop_scope();
            self.indent -= 1;
        }
        self.emit("}".to_string());
    }

    fn loop_body(&mut self, depth: usize) {
        let count = self.prng.range(1, 2);
        self.body(depth, count);
        if self.prng.chance(20) {
            let condition = self.expr(&Ty::Bool, 1);
            let jump = if self.prng.chance(50) {
                "break"
            } else {
                "continue"
            };
            self.emit(format!("if ({condition}) {{ {jump}; }}"));
        }
    }

    fn classic_for(&mut self, depth: usize) {
        let index = *self.prng.pick(&["i", "j", "step"]);
        let bound = self.prng.range(1, 3);
        self.uses(LET);
        self.binders.insert(index.to_string());
        self.emit(format!(
            "for (let {index} = 0; {index} < {bound}; {index}++) {{"
        ));
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        // The body reads the per-iteration binding and never assigns it.
        self.declare(index, Ty::Num, Decl::Local);
        self.loop_body(depth);
        self.pop_scope();
        self.indent -= 1;
        self.emit("}".to_string());
    }

    fn for_of(&mut self, depth: usize) {
        self.uses(FOR_OF);
        let iterables = self.visible_where(|b| {
            matches!(
                b.ty,
                Ty::Arr(_) | Ty::Map(_) | Ty::Set | Ty::Str | Ty::Params
            )
        });
        // The iterable, and what each iteration binds, as `(text, element
        // types, the binding the body must not touch)`.
        let (iterable, element, hidden, iterable_ty) =
            if !iterables.is_empty() && self.prng.chance(70) {
                let iterable = self.prng.pick(&iterables).clone();
                let iterable_ty = Some(iterable.ty.clone());
                let element = match iterable.ty.clone() {
                    Ty::Arr(item) => vec![*item],
                    Ty::Map(item) => {
                        self.uses(MAP);
                        vec![Ty::Str, *item]
                    }
                    Ty::Set => {
                        self.uses(SET);
                        vec![Ty::Num]
                    }
                    Ty::Params => {
                        self.uses(URL_SEARCH_PARAMS);
                        vec![Ty::Str, Ty::Str]
                    }
                    _ => vec![Ty::Str],
                };
                // Every name of the iterable's object, the aliases made before
                // the loop included.
                let names = self
                    .visible()
                    .into_iter()
                    .filter(|binding| binding.identity == iterable.identity)
                    .map(|binding| binding.name)
                    .collect::<Vec<_>>();
                (iterable.name.clone(), element, names, iterable_ty)
            } else {
                let (array, item) = self.fresh_array(1);
                (array, vec![item], Vec::new(), None)
            };
        // A head binding never names what its iterable reads: that read
        // would be in the temporal dead zone.
        let names = if element.len() == 2 {
            ["key", "value"]
                .iter()
                .all(|name| !mentions(&iterable, name))
                .then(|| vec!["key", "value"])
        } else {
            ["entry", "part", "element"]
                .into_iter()
                .find(|name| !mentions(&iterable, name))
                .map(|name| vec![name])
        };
        let Some(names) = names else {
            return self.print();
        };
        let head = if names.len() == 2 {
            self.uses(DESTRUCTURING_BINDING);
            format!("for (const [key, value] of {iterable})")
        } else {
            format!("for (const {} of {iterable})", names[0])
        };
        self.emit(format!("{head} {{"));
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        // The loop follows its iterable live (FIG-3625). The body changes it
        // only through the bounded mutation below, so no draw grows it on
        // every pass and leaves Node looping forever; it may still declare a
        // binding of the iterable's name.
        let live = iterable_ty
            .as_ref()
            .filter(|_| !hidden.is_empty() && self.prng.chance(40))
            .and_then(|ty| {
                let name = self.prng.pick(&hidden).clone();
                self.live_iterable_mutation(&name, ty)
            });
        for name in &hidden {
            self.scopes
                .last_mut()
                .expect("a scope")
                .bindings
                .push(Binding {
                    name: name.clone(),
                    ty: Ty::Num,
                    decl: Decl::Local,
                    frozen: true,
                    hidden: true,
                    identity: 0,
                });
        }
        for (name, ty) in names.into_iter().zip(element) {
            self.binders.insert(name.to_string());
            self.declare(name, ty, Decl::Local);
        }
        if let Some(mutation) = live {
            self.emit(mutation);
        }
        self.loop_body(depth);
        self.pop_scope();
        self.indent -= 1;
        self.emit("}".to_string());
    }

    /// One change to a `for...of` iterable from inside its loop, through
    /// `name` (the iterable or an alias of it), that the loop sees live. A
    /// guard bounds every growth, so the loop ends in Node as it does here.
    fn live_iterable_mutation(&mut self, name: &str, ty: &Ty) -> Option<String> {
        Some(match ty {
            Ty::Arr(item) => {
                let value = self.value_for(item);
                match self.prng.below(3) {
                    0 => format!("if ({name}.length < 6) {name}.push({value});"),
                    1 => format!("{name}.pop();"),
                    _ => format!("if ({name}.length > 0) {name}[{name}.length - 1] = {value};"),
                }
            }
            Ty::Map(item) => {
                let key = self.expr(&Ty::Str, 1);
                if self.prng.chance(50) {
                    let value = self.value_for(item);
                    format!("if ({name}.size < 6) {name}.set({key}, {value});")
                } else {
                    format!("{name}.delete({key});")
                }
            }
            Ty::Set => {
                let value = self.expr(&Ty::Num, 1);
                if self.prng.chance(50) {
                    format!("if ({name}.size < 6) {name}.add({value});")
                } else {
                    format!("{name}.delete({value});")
                }
            }
            Ty::Params => format!("{name}.delete('a');"),
            _ => return None,
        })
    }

    fn for_in(&mut self, depth: usize) {
        // An object's keys, or an array's indices.
        let objects = self.visible_where(|b| matches!(b.ty, Ty::Obj(_) | Ty::Arr(_)));
        let (object, hidden) = if objects.is_empty() {
            let (text, _) = self.fresh_object(1);
            (text, Vec::new())
        } else {
            let object = self.prng.pick(&objects).clone();
            let names = self
                .visible()
                .into_iter()
                .filter(|binding| binding.identity == object.identity)
                .map(|binding| binding.name)
                .collect::<Vec<_>>();
            (object.name, names)
        };
        self.uses(FOR_IN_ORDER);
        // The head binding never names what its object reads: that read
        // would be in the temporal dead zone.
        let Some(field) = ["field", "prop", "slot"]
            .into_iter()
            .find(|name| !mentions(&object, name))
        else {
            return self.print();
        };
        self.binders.insert(field.to_string());
        self.emit(format!("for (const {field} in {object}) {{"));
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        for name in hidden {
            // A key added while its object is being enumerated may or may
            // not be visited (ECMA-262 leaves it to the implementation), so
            // the body does not touch the object by any of its names.
            self.scopes
                .last_mut()
                .expect("a scope")
                .bindings
                .push(Binding {
                    name,
                    ty: Ty::Num,
                    decl: Decl::Local,
                    frozen: true,
                    hidden: true,
                    identity: 0,
                });
        }
        self.declare(field, Ty::Str, Decl::Local);
        if self.callbacks == 0 {
            self.emit(format!("console.log({field});"));
        }
        if self.prng.chance(40) {
            self.statement(depth + 1);
        }
        self.pop_scope();
        self.indent -= 1;
        self.emit("}".to_string());
    }

    fn while_loop(&mut self, depth: usize) {
        let counter = self.fresh_name("turn");
        let bound = self.prng.range(1, 3);
        self.uses(LET);
        self.emit(format!("let {counter} = 0;"));
        self.declare(&counter, Ty::Num, Decl::Let);
        if let Some(binding) = self.binding_mut(&counter) {
            // The loop owns its counter.
            binding.frozen = true;
        }
        let do_while = self.prng.chance(35);
        if do_while {
            self.emit("do {".to_string());
        } else {
            self.emit(format!("while ({counter} < {bound}) {{"));
        }
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        self.emit(format!("{counter}++;"));
        let count = self.prng.range(1, 2);
        self.body(depth, count);
        self.pop_scope();
        self.indent -= 1;
        if do_while {
            self.emit(format!("}} while ({counter} < {bound});"));
        } else {
            self.emit("}".to_string());
        }
    }

    fn switch(&mut self, depth: usize) {
        let subject = self.expr(&Ty::Num, 1);
        self.emit(format!("switch ({subject} % 3) {{"));
        self.indent += 1;
        let cases = self.prng.range(1, 2);
        for case in 0..cases {
            self.emit(format!("case {case}: {{"));
            self.indent += 1;
            self.push_scope(ScopeKind::Block);
            let count = self.prng.range(1, 2);
            self.body(depth, count);
            // Sometimes a case falls through into the next.
            if self.prng.chance(75) {
                self.emit("break;".to_string());
            }
            self.pop_scope();
            self.indent -= 1;
            self.emit("}".to_string());
        }
        self.emit("default: {".to_string());
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        self.statement(depth + 1);
        self.pop_scope();
        self.indent -= 1;
        self.emit("}".to_string());
        self.indent -= 1;
        self.emit("}".to_string());
    }

    fn try_statement(&mut self, depth: usize) {
        self.emit("try {".to_string());
        self.indent += 1;
        self.push_scope(ScopeKind::Block);
        // A `continue` is only ever drawn directly in a loop body, so none
        // crosses the `finally` below.
        let count = self.prng.range(1, 2);
        self.body(depth, count);
        if self.prng.chance(60) {
            let class = *self
                .prng
                .pick(&["Error", "RangeError", "TypeError", "SyntaxError"]);
            let word = self.word();
            if self.prng.chance(30) {
                self.uses(ERROR_CAUSE);
                self.emit(format!("throw new {class}({word}, {{ cause: 1 }});"));
            } else {
                self.emit(format!("throw new {class}({word});"));
            }
        }
        self.pop_scope();
        self.indent -= 1;
        if self.prng.chance(20) {
            self.uses(OPTIONAL_CATCH_BINDING);
            self.emit("} catch {".to_string());
            self.indent += 1;
            if self.callbacks == 0 {
                self.emit("console.log('caught');".to_string());
            }
        } else {
            let binding = *self.prng.pick(&["error", "problem"]);
            self.binders.insert(binding.to_string());
            self.emit(format!("}} catch ({binding}) {{"));
            self.indent += 1;
            self.push_scope(ScopeKind::Block);
            self.declare(binding, Ty::Err, Decl::Local);
            if self.callbacks == 0 {
                self.emit(format!(
                    "console.log({binding}.name, {binding}.message, {binding} instanceof RangeError);"
                ));
            }
            self.pop_scope();
        }
        self.indent -= 1;
        if self.prng.chance(40) {
            self.emit("} finally {".to_string());
            self.indent += 1;
            self.push_scope(ScopeKind::Block);
            self.statement(depth + 1);
            self.pop_scope();
            self.indent -= 1;
        }
        self.emit("}".to_string());
    }

    /// A destructuring declaration of fresh values.
    fn destructuring(&mut self) {
        let array = self.prng.chance(50);
        let value = if array {
            let a = self.expr(&Ty::Num, 1);
            let b = self.expr(&Ty::Num, 1);
            let c = self.expr(&Ty::Num, 1);
            format!("[{a}, {b}, {c}]")
        } else {
            let a = self.expr(&Ty::Str, 1);
            let b = self.expr(&Ty::Num, 1);
            format!("{{ a: {a}, b: {b} }}")
        };
        let Some(first) = self.new_name(Decl::Const, &value) else {
            return self.print();
        };
        let Some(second) = self.new_name(Decl::Const, &format!("{value} {first}")) else {
            return self.print();
        };
        self.uses(DESTRUCTURING_BINDING);
        self.uses(CONST);
        if array {
            self.emit(format!("const [{first}, ...{second}] = {value};"));
            self.declare(&first, Ty::Num, Decl::Const);
            self.declare(&second, Ty::Arr(Box::new(Ty::Num)), Decl::Const);
        } else {
            self.emit(format!(
                "const {{ a: {first}, b: {second} = 0 }} = {value};"
            ));
            self.declare(&first, Ty::Str, Decl::Const);
            self.declare(&second, Ty::Num, Decl::Const);
        }
    }

    // --- values --------------------------------------------------------------

    /// A fresh value of `ty` to store into a place.
    fn value_for(&mut self, ty: &Ty) -> String {
        self.value_for_depth(ty, 1)
    }

    /// A value for a declaration: anything, exotics included, fresh, a deep
    /// copy, or an alias of another binding's object.
    fn any_value(&mut self, depth: usize) -> (String, Ty) {
        match self.prng.below(13) {
            0..=4 => {
                let ty = self.prng.pick(&[Ty::Num, Ty::Str, Ty::Bool]).clone();
                (self.expr(&ty, depth), ty)
            }
            5..=6 => {
                let item = self.prng.pick(&[Ty::Num, Ty::Str]).clone();
                let text = self.array_of(&item, depth);
                (text, Ty::Arr(Box::new(item)))
            }
            7..=8 => self.fresh_object(depth),
            9 => {
                // An alias of another binding's object: shared identity,
                // which a reload keeps.
                let sources =
                    self.visible_where(|b| b.ty.is_compound() && !b.ty.reaches_function());
                if sources.is_empty() {
                    return self.fresh_object(depth);
                }
                let source = self.prng.pick(&sources).clone();
                self.alias_of = Some(source.identity);
                (source.name, source.ty)
            }
            10 => {
                // A deep copy of a visible value.
                let sources = self.visible_where(|b| b.ty.json_exact() && b.ty.is_compound());
                if sources.is_empty() {
                    return self.fresh_object(depth);
                }
                let source = self.prng.pick(&sources).clone();
                self.uses(BUILT_INS);
                (
                    format!("JSON.parse(JSON.stringify({}))", source.name),
                    source.ty,
                )
            }
            _ => {
                let ty = self
                    .prng
                    .pick(&[
                        Ty::Map(Box::new(Ty::Num)),
                        Ty::Set,
                        Ty::Date,
                        Ty::Re,
                        Ty::Url,
                        Ty::Params,
                        Ty::Err,
                        Ty::Match,
                    ])
                    .clone();
                (self.exotic(&ty), ty)
            }
        }
    }

    fn fresh_array(&mut self, depth: usize) -> (String, Ty) {
        let item = self.prng.pick(&[Ty::Num, Ty::Str]).clone();
        (self.array_of(&item, depth), item)
    }

    fn array_of(&mut self, item: &Ty, depth: usize) -> String {
        let length = self.prng.range(0, 3);
        match item {
            Ty::Num | Ty::Str | Ty::Bool if depth < 2 && self.prng.chance(25) => {
                // An array built from another: fresh, never an alias.
                let sources = self.visible_where(|b| b.ty == Ty::Arr(Box::new(item.clone())));
                if let Some(source) =
                    (!sources.is_empty()).then(|| self.prng.pick(&sources).clone())
                {
                    return match (item, self.prng.below(5)) {
                        (Ty::Num, 0) => self.callback_scope("value", Ty::Num, |this| {
                            let body = this.expr(&Ty::Num, 2);
                            format!("{}.map((value) => {body})", source.name)
                        }),
                        (_, 1) => {
                            self.uses(CHANGE_ARRAY_BY_COPY);
                            format!("{}.toReversed()", source.name)
                        }
                        (_, 2) => format!("{}.slice(1)", source.name),
                        (_, 3) => {
                            let extra = self.expr(item, 2);
                            format!("{}.concat([{extra}])", source.name)
                        }
                        _ => format!("[...{}]", source.name),
                    };
                }
                self.array_literal(item, length, depth)
            }
            _ => self.array_literal(item, length, depth),
        }
    }

    fn array_literal(&mut self, item: &Ty, length: usize, depth: usize) -> String {
        let items = (0..length)
            .map(|_| self.value_for_depth(item, depth + 1))
            .collect::<Vec<_>>();
        format!("[{}]", items.join(", "))
    }

    fn value_for_depth(&mut self, ty: &Ty, depth: usize) -> String {
        match ty {
            Ty::Arr(item) => self.array_of(item, depth),
            Ty::Obj(fields) => self.object_with(fields, depth),
            Ty::Num | Ty::Str | Ty::Bool => self.expr(ty, depth),
            other => self.exotic(other),
        }
    }

    /// A fresh object literal of a new shape, its keys in any order.
    fn fresh_object(&mut self, depth: usize) -> (String, Ty) {
        let count = self.prng.range(1, 3);
        let mut keys = KEYS.to_vec();
        let mut chosen = Vec::new();
        for _ in 0..count {
            let key = keys.remove(self.prng.below(keys.len()));
            chosen.push(key);
        }
        let mut fields = Vec::new();
        for key in chosen {
            let ty = if depth < 2 && self.prng.chance(20) {
                Ty::Arr(Box::new(Ty::Num))
            } else {
                self.prng.pick(&[Ty::Num, Ty::Str, Ty::Bool]).clone()
            };
            fields.push((key.to_string(), ty));
        }
        let ty = Ty::Obj(fields.clone());
        (self.object_with(&fields, depth), ty)
    }

    fn object_with(&mut self, fields: &[(String, Ty)], depth: usize) -> String {
        let computed = self.prng.chance(10);
        // Some objects are built by a spread of a literal holding their
        // leading fields, so the fields of a spread-built object are read and
        // written like any other's (FIG-3626).
        let spread = !fields.is_empty() && self.prng.chance(15);
        let parts = fields
            .iter()
            .map(|(key, ty)| {
                let value = self.value_for_depth(ty, depth + 1);
                if computed {
                    format!("['{key}']: {value}")
                } else {
                    format!("{key}: {value}")
                }
            })
            .collect::<Vec<_>>();
        if computed {
            self.uses(COMPUTED_PROPERTY_NAMES);
        }
        if parts.is_empty() {
            return "{}".to_string();
        }
        if spread {
            self.uses(OBJECT_SPREAD);
            let split = self.prng.range(1, parts.len());
            let (spread, rest) = parts.split_at(split);
            let spread = format!("...{{ {} }}", spread.join(", "));
            return format!(
                "{{ {} }}",
                std::iter::once(spread)
                    .chain(rest.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        format!("{{ {} }}", parts.join(", "))
    }

    fn exotic(&mut self, ty: &Ty) -> String {
        match ty {
            Ty::Map(_) => {
                self.uses(MAP);
                let a = self.number_literal();
                let b = self.number_literal();
                if self.prng.chance(50) {
                    format!("new Map([['b', {a}], ['a', {b}]])")
                } else {
                    self.uses(OBJECT_FROM_ENTRIES);
                    format!("new Map(Object.entries({{ z: {a}, y: {b} }}))")
                }
            }
            Ty::Set => {
                self.uses(SET);
                let a = self.number_literal();
                let b = self.number_literal();
                format!("new Set([{a}, {b}, {a}])")
            }
            Ty::Date => {
                self.uses(BUILT_INS);
                if self.prng.chance(50) {
                    let day = self.prng.range(1, 28);
                    format!("new Date(Date.UTC(2024, {}, {day}))", self.prng.below(12))
                } else {
                    format!("new Date({})", self.prng.below(4_000_000_000) * 1000)
                }
            }
            Ty::Re => {
                self.uses(BUILT_INS);
                match self.prng.below(3) {
                    0 => "/a(b*)/g".to_string(),
                    1 => "new RegExp('[a-z]+', 'i')".to_string(),
                    _ => {
                        self.uses(REGEXP_NAMED_GROUPS);
                        "/(?<head>[a-z])(?<tail>[a-z]*)/".to_string()
                    }
                }
            }
            Ty::Url => {
                self.uses(URL_CLASS);
                let path = *self.prng.pick(&["p", "a/b", "q"]);
                format!("new URL('https://example.com/{path}?x=1')")
            }
            Ty::Params => {
                self.uses(URL_SEARCH_PARAMS);
                if self.prng.chance(50) {
                    "new URLSearchParams('b=2&a=1&b=3')".to_string()
                } else {
                    "new URLSearchParams({ q: 'a b', r: '+' })".to_string()
                }
            }
            Ty::Err => {
                let class = *self.prng.pick(&["Error", "RangeError", "TypeError"]);
                let word = self.word();
                format!("new {class}({word})")
            }
            Ty::Match => {
                self.uses(REGEXP_NAMED_GROUPS);
                let word = self.word();
                format!("/(?<first>[a-z])(b)?/.exec({word})")
            }
            Ty::Fun { .. } => self.arrow(),
            Ty::Num | Ty::Str | Ty::Bool => self.expr(ty, 1),
            Ty::Arr(item) => self.array_of(item, 1),
            Ty::Obj(fields) => self.object_with(fields, 1),
        }
    }

    /// Reads of an exotic value that print identically in both engines.
    fn exotic_reads(&mut self, name: &str, ty: &Ty) -> String {
        match ty {
            Ty::Map(_) => {
                let key = self.word();
                format!(
                    "{name}.size, {name}.get('a'), {name}.has({key}), [...{name}.keys()], {name} instanceof Map"
                )
            }
            Ty::Set => {
                self.uses(SET_METHODS);
                format!(
                    "{name}.size, {name}.has(1), [...{name}.union(new Set([7]))], {name} instanceof Set"
                )
            }
            Ty::Date => format!(
                "{name}.toISOString(), {name}.getUTCDay(), {name}.getTime() > 0, {name} instanceof Date"
            ),
            Ty::Re => {
                let word = self.word();
                format!(
                    "{name}.test({word}), {name}.source, {name}.flags, {name}.lastIndex, String({name})"
                )
            }
            Ty::Url => {
                format!("{name}.href, {name}.pathname, {name}.searchParams.get('x'), {name}.origin")
            }
            Ty::Params => format!(
                "{name}.toString(), {name}.get('b'), {name}.getAll('b'), {name}.size, {name}.has('q')"
            ),
            Ty::Err => {
                format!("{name}.name, {name}.message, String({name}), {name} instanceof Error")
            }
            Ty::Match => {
                self.uses(OPTIONAL_CHAINING);
                self.uses(COALESCE);
                format!(
                    "{name}?.[0] ?? 'none', {name}?.index ?? -1, {name}?.groups?.first ?? '-', {name} === null"
                )
            }
            _ => name.to_string(),
        }
    }

    // --- expressions ---------------------------------------------------------

    fn number_literal(&mut self) -> String {
        match self.prng.below(10) {
            0 => format!("{}.5", self.prng.below(10)),
            1 => "0".to_string(),
            _ => self.prng.range(1, 12).to_string(),
        }
    }

    fn word(&mut self) -> String {
        format!("'{}'", self.prng.pick(WORDS))
    }

    fn primitive(&mut self, depth: usize) -> (String, Ty) {
        let ty = self.prng.pick(&[Ty::Num, Ty::Str, Ty::Bool]).clone();
        (self.expr(&ty, depth), ty)
    }

    /// Runs `body` inside a builtin callback's scope, where no effect may
    /// run and `param` is bound to `ty`.
    fn callback_scope(
        &mut self,
        param: &str,
        ty: Ty,
        body: impl FnOnce(&mut Self) -> String,
    ) -> String {
        self.uses(ARROW);
        self.binders.insert(param.to_string());
        self.push_scope(ScopeKind::Function);
        self.declare(param, ty, Decl::Local);
        self.callbacks += 1;
        let text = body(self);
        self.callbacks -= 1;
        self.pop_scope();
        text
    }

    /// An expression of a primitive type.
    fn expr(&mut self, ty: &Ty, depth: usize) -> String {
        let leaf = depth >= 3 || self.prng.chance(35);
        let from_binding = self.visible_where(|b| &b.ty == ty);
        if leaf {
            if !from_binding.is_empty() && self.prng.chance(60) {
                return self.prng.pick(&from_binding).name.clone();
            }
            return match ty {
                Ty::Num => self.number_literal(),
                Ty::Str => self.word(),
                _ => (*self.prng.pick(&["true", "false"])).to_string(),
            };
        }
        match ty {
            Ty::Num => self.number_expr(depth),
            Ty::Str => self.string_expr(depth),
            _ => self.bool_expr(depth),
        }
    }

    fn number_expr(&mut self, depth: usize) -> String {
        let next = depth + 1;
        match self.prng.below(19) {
            16 => {
                let left = self.expr(&Ty::Num, next);
                let right = self.expr(&Ty::Num, next);
                let op = *self.prng.pick(&["&", "|", "^", "<<", ">>", ">>>"]);
                format!("({left} {op} {right})")
            }
            17 => {
                // A spread argument, to a function of the session's own or
                // to a builtin (FIG-3627).
                let functions = self.visible_where(|b| b.ty == Ty::Fun { recursive: false });
                let arrays = self.visible_where(|b| b.ty == Ty::Arr(Box::new(Ty::Num)));
                if arrays.is_empty() {
                    return self.number_literal();
                }
                let array = self.prng.pick(&arrays).name.clone();
                if !functions.is_empty() && self.prng.chance(50) {
                    let function = self.prng.pick(&functions).name.clone();
                    return format!("{function}(...{array}, 1)");
                }
                let builtin = *self.prng.pick(&["max", "min"]);
                format!("Math.{builtin}(...{array}, 1)")
            }
            18 => {
                let sets = self.visible_where(|b| b.ty == Ty::Set);
                if let Some(set) = (!sets.is_empty()).then(|| self.prng.pick(&sets).name.clone()) {
                    self.uses(SET);
                    return format!("Array.from({set}).length");
                }
                let count = self.prng.range(0, 3);
                self.callback_scope("index", Ty::Num, |this| {
                    this.binders.insert("slot".to_string());
                    format!("Array.from({{ length: {count} }}, (slot, index) => index * 2).length")
                })
            }
            0 | 1 => {
                let left = self.expr(&Ty::Num, next);
                let right = self.expr(&Ty::Num, next);
                let op = *self.prng.pick(&["+", "-", "*", "%"]);
                format!("({left} {op} {right})")
            }
            2 => {
                self.uses(EXPONENTIATION);
                let base = self.expr(&Ty::Num, next);
                format!("({base} ** 2)")
            }
            3 => {
                let value = self.expr(&Ty::Num, next);
                let function =
                    *self
                        .prng
                        .pick(&["Math.floor", "Math.abs", "Math.round", "Math.sign"]);
                format!("{function}({value})")
            }
            4 => {
                let a = self.expr(&Ty::Num, next);
                let b = self.expr(&Ty::Num, next);
                let function = *self.prng.pick(&["Math.max", "Math.min"]);
                format!("{function}({a}, {b})")
            }
            5 => {
                let text = self.expr(&Ty::Str, next);
                format!("{text}.length")
            }
            6 => {
                let arrays = self.visible_where(|b| matches!(b.ty, Ty::Arr(_)));
                if arrays.is_empty() {
                    return self.number_literal();
                }
                let array = self.prng.pick(&arrays).clone();
                match (&array.ty, self.prng.below(4)) {
                    (Ty::Arr(item), 0) if **item == Ty::Num => {
                        let name = array.name.clone();
                        self.callback_scope("total", Ty::Num, |this| {
                            this.binders.insert("item".to_string());
                            this.declare("item", Ty::Num, Decl::Local);
                            format!("{name}.reduce((total, item) => total + item, 0)")
                        })
                    }
                    (Ty::Arr(item), 1) if **item == Ty::Num => {
                        self.uses(ARRAY_AT);
                        self.uses(COALESCE);
                        format!("({}.at(-1) ?? 0)", array.name)
                    }
                    (Ty::Arr(item), 2) => {
                        let needle = self.expr(item, next);
                        format!("{}.indexOf({needle})", array.name)
                    }
                    _ => format!("{}.length", array.name),
                }
            }
            7 => {
                let objects = self.visible_where(|b| match &b.ty {
                    Ty::Obj(fields) => fields.iter().any(|(_, ty)| *ty == Ty::Num),
                    _ => false,
                });
                if objects.is_empty() {
                    return self.number_literal();
                }
                let object = self.prng.pick(&objects).clone();
                let Ty::Obj(fields) = &object.ty else {
                    unreachable!("filtered to objects")
                };
                let numbers = fields
                    .iter()
                    .filter(|(_, ty)| *ty == Ty::Num)
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                let key = self.prng.pick(&numbers).clone();
                if self.prng.chance(30) {
                    self.uses(OPTIONAL_CHAINING);
                    format!("{}?.{key}", object.name)
                } else {
                    format!("{}.{key}", object.name)
                }
            }
            8 => {
                let functions = self.visible_where(|b| matches!(b.ty, Ty::Fun { .. }));
                if functions.is_empty() {
                    return self.number_literal();
                }
                let function = self.prng.pick(&functions).clone();
                let argument = if function.ty == (Ty::Fun { recursive: true }) {
                    self.prng.range(1, 5).to_string()
                } else {
                    self.expr(&Ty::Num, next)
                };
                format!("{}({argument})", function.name)
            }
            9 => {
                let condition = self.expr(&Ty::Bool, next);
                let a = self.expr(&Ty::Num, next);
                let b = self.expr(&Ty::Num, next);
                format!("({condition} ? {a} : {b})")
            }
            10 => {
                let text = self.expr(&Ty::Str, next);
                format!("Number.parseInt({text} + '7', 10)")
            }
            11 => {
                let exotics = self.visible_where(|b| {
                    matches!(b.ty, Ty::Map(_) | Ty::Set | Ty::Date | Ty::Params)
                });
                if exotics.is_empty() {
                    return self.number_literal();
                }
                let exotic = self.prng.pick(&exotics).clone();
                match exotic.ty {
                    Ty::Date => format!("{}.getUTCMonth()", exotic.name),
                    _ => format!("{}.size", exotic.name),
                }
            }
            12 => {
                let strings = self.visible_where(|b| b.ty == Ty::Str);
                let text = strings
                    .first()
                    .map_or_else(|| self.word(), |binding| binding.name.clone());
                format!("{text}.indexOf('b')")
            }
            13 => {
                let value = self.expr(&Ty::Num, next);
                format!("(-{value})")
            }
            _ => self.number_literal(),
        }
    }

    fn string_expr(&mut self, depth: usize) -> String {
        let next = depth + 1;
        match self.prng.below(15) {
            0 => {
                self.uses(TEMPLATE);
                let a = self.expr(&Ty::Num, next);
                let b = self.expr(&Ty::Str, next);
                format!("`${{{a}}}:${{{b}}}`")
            }
            1 => {
                let a = self.expr(&Ty::Str, next);
                let b = self.expr(&Ty::Str, next);
                format!("({a} + {b})")
            }
            2 => {
                let text = self.expr(&Ty::Str, next);
                let method = *self.prng.pick(&["toUpperCase", "toLowerCase"]);
                format!("{text}.{method}()")
            }
            3 => {
                self.uses(STRING_TRIMMING);
                let text = self.expr(&Ty::Str, next);
                let method = *self.prng.pick(&["trim", "trimStart", "trimEnd"]);
                format!("{text}.{method}()")
            }
            4 => {
                let text = self.expr(&Ty::Str, next);
                format!("{text}.slice(0, 2)")
            }
            5 => {
                let text = self.expr(&Ty::Str, next);
                format!("{text}.padStart(5, '*')")
            }
            6 => {
                let arrays = self.visible_where(|b| {
                    matches!(&b.ty, Ty::Arr(item) if matches!(**item, Ty::Num | Ty::Str | Ty::Bool))
                });
                if arrays.is_empty() {
                    return self.word();
                }
                let array = self.prng.pick(&arrays).name.clone();
                format!("{array}.join('-')")
            }
            7 => {
                let value = self.expr(&Ty::Num, next);
                format!("String({value})")
            }
            8 => {
                let value = self.expr(&Ty::Num, next);
                format!("({value}).toFixed(1)")
            }
            9 => {
                let json = self.visible_where(|b| b.ty.json_exact());
                if json.is_empty() {
                    return self.word();
                }
                let value = self.prng.pick(&json).clone();
                self.uses(BUILT_INS);
                if matches!(value.ty, Ty::Obj(_)) && self.prng.chance(40) {
                    // A spread copy with a field the original lacks,
                    // stringified whole.
                    self.uses(OBJECT_SPREAD);
                    let extra = self.expr(&Ty::Num, next);
                    return format!("JSON.stringify({{ ...{}, z: {extra} }})", value.name);
                }
                format!("JSON.stringify({})", value.name)
            }
            10 => {
                let text = self.expr(&Ty::Str, next);
                if self.prng.chance(50) {
                    self.uses(STRING_REPLACE_ALL);
                    format!("{text}.replaceAll('b', 'B')")
                } else {
                    format!("{text}.replace(/[a-z]/g, '_')")
                }
            }
            11 => {
                let text = self.expr(&Ty::Str, next);
                format!("{text}.split(',').reverse().join('|')")
            }
            12 => {
                let exotics = self.visible_where(|b| matches!(b.ty, Ty::Url | Ty::Params | Ty::Re));
                if exotics.is_empty() {
                    return self.word();
                }
                let exotic = self.prng.pick(&exotics).clone();
                match exotic.ty {
                    Ty::Url => format!("{}.search", exotic.name),
                    Ty::Params => format!("{}.toString()", exotic.name),
                    _ => format!("{}.source", exotic.name),
                }
            }
            13 => {
                let objects = self.visible_where(|b| match &b.ty {
                    Ty::Obj(fields) => fields.iter().any(|(_, ty)| *ty == Ty::Str),
                    _ => false,
                });
                if objects.is_empty() {
                    let value = self.expr(&Ty::Num, next);
                    return format!("(typeof {value})");
                }
                let object = self.prng.pick(&objects).clone();
                let Ty::Obj(fields) = &object.ty else {
                    unreachable!("filtered to objects")
                };
                let strings = fields
                    .iter()
                    .filter(|(_, ty)| *ty == Ty::Str)
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                let key = self.prng.pick(&strings).clone();
                match self.prng.below(4) {
                    0 => {
                        // A method call inside an optional chain.
                        self.uses(OPTIONAL_CHAINING);
                        format!("{}?.{key}.toUpperCase()", object.name)
                    }
                    1 => {
                        // Parentheses end the chain before the call.
                        self.uses(OPTIONAL_CHAINING);
                        format!("({}?.{key}).padEnd(4, '.')", object.name)
                    }
                    _ => format!("{}.{key}", object.name),
                }
            }
            _ => {
                let objects = self.visible_where(|b| matches!(b.ty, Ty::Obj(_)));
                if objects.is_empty() {
                    return self.word();
                }
                let object = self.prng.pick(&objects).name.clone();
                self.uses(BUILT_INS);
                format!("Object.keys({object}).join('')")
            }
        }
    }

    fn bool_expr(&mut self, depth: usize) -> String {
        let next = depth + 1;
        match self.prng.below(12) {
            0 | 1 => {
                let a = self.expr(&Ty::Num, next);
                let b = self.expr(&Ty::Num, next);
                let op = *self.prng.pick(&["<", ">=", "===", "!=="]);
                format!("({a} {op} {b})")
            }
            2 => {
                let a = self.expr(&Ty::Str, next);
                let b = self.expr(&Ty::Str, next);
                format!("({a} === {b})")
            }
            3 => {
                self.uses(STRING_INCLUDES);
                let text = self.expr(&Ty::Str, next);
                format!("{text}.includes('a')")
            }
            4 => {
                let arrays = self.visible_where(
                    |b| matches!(&b.ty, Ty::Arr(item) if matches!(**item, Ty::Num | Ty::Str)),
                );
                if arrays.is_empty() {
                    return "true".to_string();
                }
                let array = self.prng.pick(&arrays).clone();
                let Ty::Arr(item) = &array.ty else {
                    unreachable!("filtered to arrays")
                };
                let needle = self.expr(item, next);
                match self.prng.below(4) {
                    0 | 3 => {
                        self.uses(ARRAY_INCLUDES);
                        format!("{}.includes({needle})", array.name)
                    }
                    1 if **item == Ty::Num => {
                        let name = array.name.clone();
                        self.callback_scope("value", Ty::Num, |this| {
                            let limit = this.expr(&Ty::Num, next + 1);
                            format!("{name}.some((value) => value > {limit})")
                        })
                    }
                    _ => format!("Array.isArray({})", array.name),
                }
            }
            5 => {
                let value = self.expr(&Ty::Bool, next);
                format!("!{value}")
            }
            6 => {
                let a = self.expr(&Ty::Bool, next);
                let b = self.expr(&Ty::Bool, next);
                let op = *self.prng.pick(&["&&", "||"]);
                format!("({a} {op} {b})")
            }
            7 => {
                let objects = self.visible_where(|b| matches!(b.ty, Ty::Obj(_)));
                if objects.is_empty() {
                    return "false".to_string();
                }
                let object = self.prng.pick(&objects).name.clone();
                let key = *self.prng.pick(KEYS);
                if self.prng.chance(50) {
                    self.uses(OBJECT_HAS_OWN);
                    format!("Object.hasOwn({object}, '{key}')")
                } else {
                    format!("('{key}' in {object})")
                }
            }
            8 => {
                let exotics = self
                    .visible_where(|b| matches!(b.ty, Ty::Map(_) | Ty::Set | Ty::Re | Ty::Params));
                if exotics.is_empty() {
                    return "true".to_string();
                }
                let exotic = self.prng.pick(&exotics).clone();
                match exotic.ty {
                    Ty::Map(_) => format!("{}.has('a')", exotic.name),
                    Ty::Set => format!("{}.has(2)", exotic.name),
                    Ty::Params => format!("{}.has('b')", exotic.name),
                    _ => {
                        let word = self.word();
                        format!("{}.test({word})", exotic.name)
                    }
                }
            }
            9 => {
                let values = self.visible_where(|b| !b.ty.reaches_function());
                if values.is_empty() {
                    return "false".to_string();
                }
                let value = self.prng.pick(&values).clone();
                let class = *self.prng.pick(&["Array", "Object", "Map", "Date", "Error"]);
                format!("({} instanceof {class})", value.name)
            }
            10 => {
                self.uses(ARRAY_FIND_FROM_LAST);
                let arrays = self.visible_where(|b| b.ty == Ty::Arr(Box::new(Ty::Num)));
                if arrays.is_empty() {
                    return "false".to_string();
                }
                let array = self.prng.pick(&arrays).name.clone();
                self.callback_scope("value", Ty::Num, |this| {
                    let limit = this.expr(&Ty::Num, next + 1);
                    format!("({array}.findLastIndex((value) => value < {limit}) >= 0)")
                })
            }
            _ => {
                self.uses(ARRAY_FLAT);
                let a = self.expr(&Ty::Num, next);
                format!("[[{a}], [2]].flat().includes(2)")
            }
        }
    }
}

/// Whether `text` reads `name` as an identifier.
pub(super) fn mentions(text: &str, name: &str) -> bool {
    let identifier =
        |character: char| character.is_ascii_alphanumeric() || matches!(character, '_' | '$');
    text.match_indices(name).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + name.len()..].chars().next();
        !before.is_some_and(identifier) && !after.is_some_and(identifier)
    })
}
