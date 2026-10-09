//! The executable: an admitted document compiled to tables the machine
//! walks.
//!
//! The executable is a cache (`docs/kernel/design.md` §5). It is derived
//! deterministically from the document and the registry, is never saved,
//! and nothing outside this crate sees its shape. Names are resolved to
//! frame slots, declared and library functions to table indexes, and every
//! node that an identity names keeps its site.
//!
//! A [`Layout`] permutes the tables and the frame slots. No value, effect
//! identity or charge may depend on it; the laws compile one document under
//! several layouts and compare the runs.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc as doc;
use lash_kernel_doc::{
    Document, FunctionDefinition, FunctionId, FunctionRegistry, JoinMode, Name, NativeFunction,
    Site, Type, Unit, Value,
};

use crate::functions::{MachineFunction, machine_function};

/// How the executable is laid out. Layout `0` is the natural order; any
/// other value permutes the code, block and statement tables and each
/// frame's slots. It exists for tests: every layout runs a document the
/// same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CodeId(pub(crate) u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BlockId(pub(crate) u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StmtId(pub(crate) u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LibId(pub(crate) u32);
/// A variable of a code, by declaration order.
pub(crate) type Slot = u32;

pub(crate) struct Executable {
    pub(crate) codes: Vec<Code>,
    pub(crate) blocks: Vec<Block>,
    pub(crate) stmts: Vec<Stmt>,
    pub(crate) libs: Vec<Lib>,
    pub(crate) main: CodeId,
    pub(crate) declared: BTreeMap<Name, CodeId>,
}

/// One function body: `main`, a declared function, a library body or a
/// closure.
pub(crate) struct Code {
    pub(crate) params: Vec<Slot>,
    pub(crate) body: BlockId,
    pub(crate) slots: Vec<SlotInfo>,
    /// Where each slot sits in a frame.
    pub(crate) positions: Vec<u32>,
    /// The enclosing code's variables this closure shares.
    pub(crate) captures: Vec<Capture>,
    /// Whether the code's forms are charged: a library body's are not
    /// (`K-CHG-007`).
    pub(crate) charged: bool,
}

pub(crate) struct SlotInfo {
    pub(crate) name: Name,
    /// A closure shares the variable, so it lives in a heap cell.
    pub(crate) shared: bool,
}

pub(crate) struct Capture {
    /// The slot in the enclosing code.
    pub(crate) outer: Slot,
    /// The slot in this code.
    pub(crate) inner: Slot,
}

pub(crate) struct Block {
    pub(crate) stmts: Vec<StmtId>,
    /// The variables the block declares; they end with it.
    pub(crate) declares: Vec<Slot>,
}

pub(crate) struct Lib {
    pub(crate) id: FunctionId,
    pub(crate) definition: Arc<FunctionDefinition>,
    pub(crate) run: LibRun,
}

pub(crate) enum LibRun {
    Native(Arc<dyn NativeFunction>),
    Body(CodeId),
    Machine(MachineFunction),
}

pub(crate) enum Var {
    Local(Slot),
    /// A session binding, looked up by name when it is read
    /// (`K-SES-001`).
    Session(Name),
    /// A name no enclosing scope declares (`K-FORM-003`).
    Unbound(Name),
}

pub(crate) enum Stmt {
    Let {
        target: Target,
        value: Rhs,
    },
    Assign {
        place: Place,
        value: Rhs,
    },
    Remove(Member),
    Do(Action),
    If {
        condition: Expr,
        then_block: BlockId,
        else_block: BlockId,
    },
    For {
        site: Site,
        binding: Slot,
        iterable: Expr,
        body: BlockId,
    },
    While {
        site: Site,
        condition: Expr,
        body: BlockId,
    },
    Break,
    Continue,
    Return(Expr),
    Try {
        body: BlockId,
        catch: Option<(Slot, BlockId)>,
        finally: Option<BlockId>,
    },
    Throw(Expr),
    Print(Expr),
    Finish(Expr),
    Fail(Expr),
}

pub(crate) enum Target {
    Slot(Slot),
    Session(Name),
}

pub(crate) enum Rhs {
    Expr(Expr),
    Action(Action),
}

pub(crate) enum Place {
    Var(Var),
    Member(Member),
}

pub(crate) enum Member {
    Field(Expr, String),
    Index(Expr, Expr),
}

pub(crate) struct Action {
    pub(crate) site: Site,
    pub(crate) kind: ActionKind,
}

pub(crate) enum ActionKind {
    Call {
        callee: Callee,
        args: Vec<Atom>,
    },
    Perform {
        effect: doc::EffectName,
        args: Vec<Atom>,
        result: Type,
    },
    Sleep(Atom),
    Join(Atom),
    JoinMany(JoinMode, Atom),
    Yield,
    Spawn {
        callee: Callee,
        args: Vec<Atom>,
    },
    Cancel(Atom),
}

pub(crate) enum Callee {
    Declared(CodeId),
    Value(Var),
    Library(LibId),
}

pub(crate) enum Atom {
    Var(Var),
    Literal(Value),
}

pub(crate) enum Expr {
    Literal(Value),
    Var(Var),
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Set(Vec<Expr>),
    Record(Vec<(String, Expr)>),
    Member(Box<Member>),
    Closure(CodeId),
    Call { lib: LibId, args: Vec<Expr> },
    Clock,
    Random,
    Read(Box<(Expr, Expr)>),
}

/// A program the machine cannot compile: a function the registry lacks.
pub(crate) struct Missing(pub(crate) FunctionId);

/// A small deterministic generator for layouts (splitmix64).
struct Shuffler(u64);

impl Shuffler {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn shuffle<T>(&mut self, layout: Layout, items: &mut [T]) {
        if layout.0 == 0 {
            return;
        }
        for last in (1..items.len()).rev() {
            let pick = (self.next() % (last as u64 + 1)) as usize;
            items.swap(last, pick);
        }
    }
}

/// The scopes of one code while it is compiled.
struct Scopes {
    main: bool,
    /// The innermost block last; in each, the latest declaration last.
    blocks: Vec<Vec<(Name, Declared)>>,
    slots: Vec<SlotInfo>,
    captures: Vec<Capture>,
}

#[derive(Clone, Copy)]
enum Declared {
    Slot(Slot),
    Session,
}

struct Compiler<'a> {
    document: &'a Document,
    layout: Layout,
    shuffler: Shuffler,
    codes: Vec<Option<Code>>,
    blocks: Vec<Block>,
    stmts: Vec<Stmt>,
    declared: BTreeMap<Name, CodeId>,
    libs: BTreeMap<FunctionId, LibId>,
    /// The codes being compiled, the outermost first; each closure adds one.
    scopes: Vec<Scopes>,
    unit: Unit,
    path: Vec<u32>,
}

enum UnitSource<'a> {
    Main,
    Function(&'a Name),
    Library(FunctionId, &'a Arc<FunctionDefinition>),
}

pub(crate) fn compile(
    document: &Document,
    registry: &FunctionRegistry,
    layout: Layout,
) -> Result<Executable, Missing> {
    // Every library function the document reaches, through bodies too.
    let mut reached: BTreeMap<FunctionId, &doc::RegisteredFunction> = BTreeMap::new();
    let mut pending: Vec<FunctionId> = document.manifest.functions.keys().copied().collect();
    while let Some(function) = pending.pop() {
        if reached.contains_key(&function) {
            continue;
        }
        let registered = registry.get(&function).ok_or(Missing(function))?;
        if registered.native.is_none()
            && let Some(body) = registered.definition.body()
        {
            pending.extend(body.functions.keys().copied());
        }
        reached.insert(function, registered);
    }

    let mut shuffler = Shuffler(layout.0);
    let mut library: Vec<(FunctionId, &doc::RegisteredFunction)> = reached.into_iter().collect();
    shuffler.shuffle(layout, &mut library);
    let mut units: Vec<UnitSource<'_>> = vec![UnitSource::Main];
    units.extend(document.functions.keys().map(UnitSource::Function));
    let mut libs = Vec::with_capacity(library.len());
    let mut lib_ids = BTreeMap::new();
    for (index, (function, registered)) in library.iter().enumerate() {
        lib_ids.insert(*function, LibId(index as u32));
        // Filled in below, once every unit has its code id.
        libs.push((*function, *registered));
        let runs_body = registered.native.is_none()
            && machine_function(&registered.definition).is_none()
            && registered.definition.body().is_some();
        if runs_body {
            units.push(UnitSource::Library(*function, &registered.definition));
        }
    }
    shuffler.shuffle(layout, &mut units);

    let mut compiler = Compiler {
        document,
        layout,
        shuffler,
        codes: Vec::new(),
        blocks: Vec::new(),
        stmts: Vec::new(),
        declared: BTreeMap::new(),
        libs: lib_ids,
        scopes: Vec::new(),
        unit: Unit::Main,
        path: Vec::new(),
    };
    let mut main = CodeId(0);
    let mut bodies: BTreeMap<FunctionId, CodeId> = BTreeMap::new();
    for (index, unit) in units.iter().enumerate() {
        let code = CodeId(index as u32);
        compiler.codes.push(None);
        match unit {
            UnitSource::Main => main = code,
            UnitSource::Function(name) => {
                compiler.declared.insert((*name).clone(), code);
            }
            UnitSource::Library(function, _) => {
                bodies.insert(*function, code);
            }
        }
    }
    for (index, unit) in units.iter().enumerate() {
        let code = CodeId(index as u32);
        match unit {
            UnitSource::Main => compiler.unit_code(code, Unit::Main, &[], &document.main, true),
            UnitSource::Function(name) => {
                if let Some(function) = document.functions.get(*name) {
                    let unit = Unit::Function((*name).clone());
                    compiler.unit_code(code, unit, &function.params, &function.body, true);
                }
            }
            UnitSource::Library(function, definition) => {
                if let Some(body) = definition.body() {
                    let params: Vec<Name> = definition
                        .signature
                        .params
                        .iter()
                        .map(|param| param.name.clone())
                        .collect();
                    let unit = Unit::Library(*function);
                    compiler.unit_code(code, unit, &params, &body.block, false);
                }
            }
        }
    }

    let libs = libs
        .into_iter()
        .map(|(function, registered)| {
            let definition = Arc::clone(&registered.definition);
            let run = if let Some(machine) = machine_function(&definition) {
                LibRun::Machine(machine)
            } else if let Some(native) = &registered.native {
                LibRun::Native(Arc::clone(native))
            } else {
                // A function with no native registered has a body, and the
                // loop above gave every such body a code.
                LibRun::Body(bodies.get(&function).copied().unwrap_or(main))
            };
            Lib {
                id: function,
                definition,
                run,
            }
        })
        .collect();
    Ok(Executable {
        codes: compiler.codes.into_iter().flatten().collect(),
        blocks: compiler.blocks,
        stmts: compiler.stmts,
        libs,
        main,
        declared: compiler.declared,
    })
}

impl Compiler<'_> {
    fn site(&self) -> Site {
        Site::new(self.unit.clone(), self.path.clone())
    }

    /// Compiles one unit's body into the code slot reserved for it.
    fn unit_code(
        &mut self,
        code: CodeId,
        unit: Unit,
        params: &[Name],
        body: &doc::Block,
        charged: bool,
    ) {
        let main = unit == Unit::Main;
        self.unit = unit;
        self.path.clear();
        let compiled = self.code(params, body, main, charged);
        self.codes[code.0 as usize] = Some(compiled);
    }

    /// Compiles a function body in a scope of its own. The path addresses
    /// the body block.
    fn code(&mut self, params: &[Name], body: &doc::Block, main: bool, charged: bool) -> Code {
        self.scopes.push(Scopes {
            main,
            blocks: Vec::new(),
            slots: Vec::new(),
            captures: Vec::new(),
        });
        let (body, params) = self.block(body, params);
        let scopes = self.scopes.pop().unwrap_or(Scopes {
            main,
            blocks: Vec::new(),
            slots: Vec::new(),
            captures: Vec::new(),
        });
        let mut positions: Vec<u32> = (0..scopes.slots.len() as u32).collect();
        self.shuffler.shuffle(self.layout, &mut positions);
        Code {
            params,
            body,
            slots: scopes.slots,
            positions,
            captures: scopes.captures,
            charged,
        }
    }

    fn declare(&mut self, name: &Name) -> Slot {
        let Some(scopes) = self.scopes.last_mut() else {
            return 0;
        };
        let slot = scopes.slots.len() as Slot;
        scopes.slots.push(SlotInfo {
            name: name.clone(),
            shared: false,
        });
        if let Some(block) = scopes.blocks.last_mut() {
            block.push((name.clone(), Declared::Slot(slot)));
        }
        slot
    }

    /// Compiles a block, declaring `bound` in its scope first: a function's
    /// parameters, a loop's binding, a catch's binding.
    fn block(&mut self, block: &doc::Block, bound: &[Name]) -> (BlockId, Vec<Slot>) {
        if let Some(scopes) = self.scopes.last_mut() {
            scopes.blocks.push(Vec::new());
        }
        let bound: Vec<Slot> = bound.iter().map(|name| self.declare(name)).collect();
        let mut stmts = Vec::with_capacity(block.len());
        for (index, stmt) in block.iter().enumerate() {
            self.path.push(index as u32);
            let stmt = self.stmt(stmt);
            self.path.pop();
            stmts.push(stmt);
        }
        let declared = self
            .scopes
            .last_mut()
            .and_then(|scopes| scopes.blocks.pop())
            .unwrap_or_default();
        let declares = declared
            .into_iter()
            .filter_map(|(_, declared)| match declared {
                Declared::Slot(slot) => Some(slot),
                Declared::Session => None,
            })
            .collect();
        // Statement and block ids are taken in the order the layout visits
        // the units, so they differ between layouts.
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block {
            stmts: Vec::new(),
            declares,
        });
        let ids = stmts
            .into_iter()
            .map(|stmt| {
                let id = StmtId(self.stmts.len() as u32);
                self.stmts.push(stmt);
                id
            })
            .collect();
        self.blocks[id.0 as usize].stmts = ids;
        (id, bound)
    }

    fn child<T>(&mut self, index: u32, compile: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(index);
        let compiled = compile(self);
        self.path.pop();
        compiled
    }

    fn stmt(&mut self, stmt: &doc::Stmt) -> Stmt {
        match stmt {
            doc::Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                let value = self.child(0, |c| c.rhs(value));
                let top_level = self.scopes.len() == 1
                    && self
                        .scopes
                        .last()
                        .is_some_and(|scopes| scopes.main && scopes.blocks.len() == 1);
                let target = if top_level && !self.document.private_bindings.contains(name) {
                    if let Some(block) = self.scopes.last_mut().and_then(|s| s.blocks.last_mut()) {
                        block.push((name.clone(), Declared::Session));
                    }
                    Target::Session(name.clone())
                } else {
                    Target::Slot(self.declare(name))
                };
                Stmt::Let { target, value }
            }
            doc::Stmt::Assign { place, value } => {
                let (place, next) = match place {
                    doc::Place::Variable(name) => (Place::Var(self.resolve(name)), 0),
                    doc::Place::Member(member) => {
                        let (member, next) = self.member(member, 0);
                        (Place::Member(member), next)
                    }
                };
                let value = self.child(next, |c| c.rhs(value));
                Stmt::Assign { place, value }
            }
            doc::Stmt::Remove { member } => Stmt::Remove(self.member(member, 0).0),
            doc::Stmt::Do { action } => Stmt::Do(self.child(0, |c| c.action(action))),
            doc::Stmt::If {
                condition,
                then_block,
                else_block,
            } => Stmt::If {
                condition: self.child(0, |c| c.expr(condition)),
                then_block: self.child(1, |c| c.block(then_block, &[]).0),
                else_block: self.child(2, |c| c.block(else_block, &[]).0),
            },
            doc::Stmt::For {
                binding,
                iterable,
                body,
            } => {
                let iterable = self.child(0, |c| c.expr(iterable));
                let (body, bound) = self.child(1, |c| c.block(body, std::slice::from_ref(binding)));
                Stmt::For {
                    site: self.site(),
                    binding: bound.first().copied().unwrap_or(0),
                    iterable,
                    body,
                }
            }
            doc::Stmt::While { condition, body } => Stmt::While {
                site: self.site(),
                condition: self.child(0, |c| c.expr(condition)),
                body: self.child(1, |c| c.block(body, &[]).0),
            },
            doc::Stmt::Break => Stmt::Break,
            doc::Stmt::Continue => Stmt::Continue,
            doc::Stmt::Return { value } => Stmt::Return(self.child(0, |c| c.expr(value))),
            doc::Stmt::Throw { value } => Stmt::Throw(self.child(0, |c| c.expr(value))),
            doc::Stmt::Print { value } => Stmt::Print(self.child(0, |c| c.expr(value))),
            doc::Stmt::Finish { value } => Stmt::Finish(self.child(0, |c| c.expr(value))),
            doc::Stmt::Fail { value } => Stmt::Fail(self.child(0, |c| c.expr(value))),
            doc::Stmt::Try(scope) => {
                let mut next = 0;
                let body = self.child(next, |c| c.block(&scope.body, &[]).0);
                next += 1;
                let catch = scope.catch.as_ref().map(|catch| {
                    let (block, bound) = self.child(next, |c| {
                        c.block(&catch.body, std::slice::from_ref(&catch.binding))
                    });
                    next += 1;
                    (bound.first().copied().unwrap_or(0), block)
                });
                let finally = scope
                    .finally
                    .as_ref()
                    .map(|finally| self.child(next, |c| c.block(finally, &[]).0));
                Stmt::Try {
                    body,
                    catch,
                    finally,
                }
            }
        }
    }

    fn rhs(&mut self, rhs: &doc::Rhs) -> Rhs {
        match rhs {
            doc::Rhs::Expr(expr) => Rhs::Expr(self.expr(expr)),
            doc::Rhs::Action(action) => Rhs::Action(self.action(action)),
        }
    }

    /// Compiles a member whose first child has index `first`, and returns
    /// the index after its last.
    fn member(&mut self, member: &doc::Member, first: u32) -> (Member, u32) {
        match member {
            doc::Member::Field { target, field } => (
                Member::Field(self.child(first, |c| c.expr(target)), field.clone()),
                first + 1,
            ),
            doc::Member::Index { target, index } => (
                Member::Index(
                    self.child(first, |c| c.expr(target)),
                    self.child(first + 1, |c| c.expr(index)),
                ),
                first + 2,
            ),
        }
    }

    fn action(&mut self, action: &doc::Action) -> Action {
        let kind = match action {
            doc::Action::Call { callee, args } => ActionKind::Call {
                callee: self.callee(callee),
                args: self.atoms(args),
            },
            doc::Action::Spawn { callee, args } => ActionKind::Spawn {
                callee: self.callee(callee),
                args: self.atoms(args),
            },
            doc::Action::Perform {
                effect,
                args,
                result,
            } => ActionKind::Perform {
                effect: effect.clone(),
                args: self.atoms(args),
                result: result.clone(),
            },
            doc::Action::Sleep { duration } => ActionKind::Sleep(self.atom(duration)),
            doc::Action::Join { task } => ActionKind::Join(self.atom(task)),
            doc::Action::JoinMany { mode, tasks } => ActionKind::JoinMany(*mode, self.atom(tasks)),
            doc::Action::Yield => ActionKind::Yield,
            doc::Action::Cancel { task } => ActionKind::Cancel(self.atom(task)),
        };
        Action {
            site: self.site(),
            kind,
        }
    }

    fn callee(&mut self, callee: &doc::Callee) -> Callee {
        match callee {
            doc::Callee::Declared(name) => match self.declared.get(name) {
                Some(code) => Callee::Declared(*code),
                // Validation refuses this; a variable of that name is what
                // is left to mean.
                None => Callee::Value(Var::Unbound(name.clone())),
            },
            doc::Callee::Value(name) => Callee::Value(self.resolve(name)),
            doc::Callee::Library(function) => match self.libs.get(function) {
                Some(lib) => Callee::Library(*lib),
                None => Callee::Value(Var::Unbound(Name::new(function.to_string()))),
            },
        }
    }

    fn atoms(&mut self, atoms: &[doc::Atom]) -> Vec<Atom> {
        atoms.iter().map(|atom| self.atom(atom)).collect()
    }

    fn atom(&mut self, atom: &doc::Atom) -> Atom {
        match atom {
            doc::Atom::Variable(name) => Atom::Var(self.resolve(name)),
            doc::Atom::Literal(literal) => Atom::Literal(literal_value(literal)),
        }
    }

    fn exprs(&mut self, exprs: &[doc::Expr]) -> Vec<Expr> {
        exprs
            .iter()
            .enumerate()
            .map(|(index, expr)| self.child(index as u32, |c| c.expr(expr)))
            .collect()
    }

    fn expr(&mut self, expr: &doc::Expr) -> Expr {
        match expr {
            doc::Expr::Literal(literal) => Expr::Literal(literal_value(literal)),
            doc::Expr::Variable(name) => Expr::Var(self.resolve(name)),
            doc::Expr::Tuple(items) => Expr::Tuple(self.exprs(items)),
            doc::Expr::List(items) => Expr::List(self.exprs(items)),
            doc::Expr::Set(items) => Expr::Set(self.exprs(items)),
            doc::Expr::Map(entries) => Expr::Map(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let index = index as u32 * 2;
                        (
                            self.child(index, |c| c.expr(&entry.key)),
                            self.child(index + 1, |c| c.expr(&entry.value)),
                        )
                    })
                    .collect(),
            ),
            doc::Expr::Record(entries) => Expr::Record(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let value = self.child(index as u32, |c| c.expr(&entry.value));
                        (entry.field.clone(), value)
                    })
                    .collect(),
            ),
            doc::Expr::Member(member) => Expr::Member(Box::new(self.member(member, 0).0)),
            doc::Expr::Closure(closure) => {
                let charged = !matches!(self.unit, Unit::Library(_));
                let code = self.child(0, |c| {
                    c.code(&closure.params, &closure.body, false, charged)
                });
                let id = CodeId(self.codes.len() as u32);
                self.codes.push(Some(code));
                Expr::Closure(id)
            }
            doc::Expr::Call { function, args } => match self.libs.get(function).copied() {
                Some(lib) => Expr::Call {
                    lib,
                    args: self.exprs(args),
                },
                None => Expr::Var(Var::Unbound(Name::new(function.to_string()))),
            },
            doc::Expr::Clock => Expr::Clock,
            doc::Expr::Random => Expr::Random,
            doc::Expr::Read(read) => Expr::Read(Box::new((
                self.child(0, |c| c.expr(&read.handle)),
                self.child(1, |c| c.expr(&read.request)),
            ))),
        }
    }

    fn resolve(&mut self, name: &Name) -> Var {
        match self.scopes.len() {
            0 => Var::Unbound(name.clone()),
            depth => self.resolve_in(depth - 1, name),
        }
    }

    /// Resolves `name` as code `level` sees it. A variable of an enclosing
    /// code becomes a capture of every closure between the two.
    fn resolve_in(&mut self, level: usize, name: &Name) -> Var {
        let scopes = &self.scopes[level];
        let declared = scopes
            .blocks
            .iter()
            .rev()
            .flat_map(|block| block.iter().rev())
            .find(|(declared, _)| declared == name)
            .map(|(_, declared)| *declared);
        match declared {
            Some(Declared::Slot(slot)) => return Var::Local(slot),
            Some(Declared::Session) => return Var::Session(name.clone()),
            None => {}
        }
        if level == 0 {
            return if scopes.main {
                Var::Session(name.clone())
            } else {
                Var::Unbound(name.clone())
            };
        }
        // A capture already taken for this name is the same variable: the
        // closure sees its defining scope as it was at the closure.
        let taken = scopes
            .captures
            .iter()
            .find(|capture| scopes.slots[capture.inner as usize].name == *name)
            .map(|capture| capture.inner);
        if let Some(inner) = taken {
            return Var::Local(inner);
        }
        match self.resolve_in(level - 1, name) {
            Var::Local(outer) => {
                self.scopes[level - 1].slots[outer as usize].shared = true;
                let scopes = &mut self.scopes[level];
                let inner = scopes.slots.len() as Slot;
                scopes.slots.push(SlotInfo {
                    name: name.clone(),
                    shared: true,
                });
                scopes.captures.push(Capture { outer, inner });
                Var::Local(inner)
            }
            other => other,
        }
    }
}

fn literal_value(literal: &doc::Literal) -> Value {
    match literal {
        doc::Literal::Null => Value::Null,
        doc::Literal::Absent => Value::Absent,
        doc::Literal::Bool(flag) => Value::Bool(*flag),
        doc::Literal::Int(integer) => Value::Int(integer.clone()),
        doc::Literal::Float(float) => Value::Float(*float),
        doc::Literal::Text(text) => Value::text(text.as_str()),
        doc::Literal::Bytes(bytes) => Value::Bytes(bytes.clone()),
        doc::Literal::Function(name) => Value::Function(name.clone()),
    }
}
