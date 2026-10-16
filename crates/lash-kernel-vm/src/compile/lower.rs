//! Lowering: each statement of a document or a library body to its range
//! of the flat stream (`lash_kernel_exec::Op`).
//!
//! A statement's expressions lower in evaluation order, each node's value
//! to the register its depth names, after an explicit `Charge` at the
//! node's pre-order position (`K-CHG-002`): a node is charged before its
//! sub-expressions are evaluated, so a bound crossed at the node ends the
//! run before anything under it can raise. Adjacent charges coalesce; the
//! machine charges a run that crosses its bound inside them up to the
//! event that crosses. Children compile in their child order, so closures
//! take their codes in the order the document's sites give them.
//!
//! Each statement's ops are gathered apart and appended whole when the
//! statement is done, after the statements of the blocks it holds, so
//! every statement is one contiguous range. Its variables are placed once
//! its code is complete (`super::Placer`).

use lash_kernel_doc as doc;
use lash_kernel_doc::Name;
use lash_kernel_exec::{
    BlockIx, CodeIx, ConstIx, FieldIx, FieldListIx, LibIx, LocalRef, NameIx, Op, OpRange, Reg,
    RegRange,
};

use super::{Action, ActionKind, Atom, Callee, CodeId, Compiler, Form, Local, Stmt, Var};

/// One statement's ops as they are gathered, and how many registers they
/// use.
struct Ops {
    ops: Vec<Op>,
    regs: u32,
    /// Whether a charge joins the read after it, as an even layout's
    /// stream runs them.
    fuse: bool,
}

impl Ops {
    fn push(&mut self, op: Op) {
        if self.fuse
            && let Some(Op::Charge { units }) = self.ops.last()
            && let Ok(units) = u8::try_from(*units)
        {
            let fused = match op {
                Op::Local { dst, local } => Some(Op::ChargeLocal { units, dst, local }),
                Op::Const { dst, c } => Some(Op::ChargeConst { units, dst, c }),
                _ => None,
            };
            if let (Some(fused), Some(last)) = (fused, self.ops.last_mut()) {
                *last = fused;
                return;
            }
        }
        self.ops.push(op);
    }

    /// One more one-unit charge event, joined to the one before it when
    /// nothing comes between them.
    fn charge(&mut self) {
        match self.ops.last_mut() {
            Some(Op::Charge { units }) => *units += 1,
            _ => self.ops.push(Op::Charge { units: 1 }),
        }
    }

    /// Notes that register `reg` is used.
    fn uses(&mut self, reg: Reg) {
        self.regs = self.regs.max(reg.0 + 1);
    }
}

/// How an assignment's member target lowers: its field, or an index in
/// the register after the target.
enum Target {
    Field(FieldIx),
    Index,
}

impl Compiler<'_> {
    pub(super) fn stmt(&mut self, stmt: &doc::Stmt) -> Stmt {
        let mut ops = self.ops();
        let mut form = match stmt {
            doc::Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                let form = self.child(0, |c| c.rhs(&mut ops, value));
                let top_level = self.scopes.len() == 1
                    && self
                        .scopes
                        .last()
                        .is_some_and(|scopes| scopes.main && scopes.blocks.len() == 1);
                if top_level && !self.private.contains(name) {
                    if let Some(block) = self.scopes.last_mut().and_then(|s| s.blocks.last_mut()) {
                        block.push((name.clone(), super::Declared::Session));
                    }
                    let name = self.name(name.clone());
                    ops.push(Op::BindSession { name, src: Reg(0) });
                } else {
                    let slot = self.declare(name);
                    ops.push(Op::Bind {
                        local: LocalRef { slot, at: slot },
                        src: Reg(0),
                    });
                }
                form
            }
            doc::Stmt::Assign { place, value } => match place {
                doc::Place::Variable(name) => {
                    let var = self.resolve(name);
                    let form = self.child(0, |c| c.rhs(&mut ops, value));
                    let src = Reg(0);
                    ops.push(match var {
                        Var::Local(local) => Op::WriteLocal {
                            local: local_ref(local),
                            src,
                        },
                        Var::Session(name) => Op::WriteSession {
                            name: self.name(name),
                            src,
                        },
                        Var::Unbound(name) => Op::WriteUnbound {
                            name: self.name(name),
                            src,
                        },
                    });
                    form
                }
                doc::Place::Member(member) => {
                    // The target and the index are evaluated after the
                    // right-hand side, which register 0 holds meanwhile
                    // (`K-FORM-005`, `K-FORM-006`); they compile first, as
                    // their children come first.
                    let mut written = self.ops();
                    let (field, next) = self.member(&mut written, member, 0, Reg(1));
                    let form = self.child(next, |c| c.rhs(&mut ops, value));
                    ops.push(Op::Pin { src: Reg(0) });
                    ops.regs = ops.regs.max(written.regs);
                    ops.ops.append(&mut written.ops);
                    self.spare.push(written.ops);
                    let (target, src) = (Reg(1), Reg(0));
                    ops.push(match target_of(field) {
                        Target::Field(field) => Op::SetField { target, field, src },
                        Target::Index => Op::SetIndex { target, src },
                    });
                    form
                }
            },
            doc::Stmt::Remove { member } => {
                let target = Reg(0);
                let (field, _) = self.member(&mut ops, member, 0, target);
                ops.push(match target_of(field) {
                    Target::Field(field) => Op::RemoveField { target, field },
                    Target::Index => Op::RemoveIndex { target },
                });
                Form::Plain
            }
            doc::Stmt::Do { action } => {
                let action = self.child(0, |c| c.action(action));
                ops.push(Op::Act);
                ops.uses(Reg(0));
                ops.push(Op::End);
                Form::Action(action)
            }
            doc::Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.child(0, |c| c.expr(&mut ops, condition, Reg(0)));
                let then_block = self.child(1, |c| c.block(then_block, &[]).0);
                let else_block = self.child(2, |c| c.block(else_block, &[]).0);
                ops.push(Op::Branch {
                    cond: Reg(0),
                    then_: BlockIx(then_block.0),
                    else_: BlockIx(else_block.0),
                });
                Form::If {
                    then_block,
                    else_block,
                }
            }
            doc::Stmt::For {
                binding,
                iterable,
                body,
            } => {
                self.child(0, |c| c.expr(&mut ops, iterable, Reg(0)));
                let (body, bound) = self.child(1, |c| c.block(body, std::slice::from_ref(binding)));
                ops.push(Op::ForIn { iterable: Reg(0) });
                Form::For {
                    site: self.site(),
                    binding: Local::unplaced(bound.first().copied().unwrap_or(0)),
                    body,
                }
            }
            doc::Stmt::While { condition, body } => {
                // The condition's ops follow the `while`'s own: its test
                // range, which the machine places once the range is known.
                ops.push(Op::While);
                self.child(0, |c| c.expr(&mut ops, condition, Reg(0)));
                ops.push(Op::Test { cond: Reg(0) });
                Form::While {
                    site: self.site(),
                    test: OpRange { start: 1, len: 0 },
                    body: self.child(1, |c| c.block(body, &[]).0),
                }
            }
            doc::Stmt::Break => {
                ops.push(Op::Break);
                Form::Plain
            }
            doc::Stmt::Continue => {
                ops.push(Op::Continue);
                Form::Plain
            }
            doc::Stmt::Return { value } => self.valued(&mut ops, value, |src| Op::Return { src }),
            doc::Stmt::Throw { value } => self.valued(&mut ops, value, |src| Op::Throw { src }),
            doc::Stmt::Print { value } => self.valued(&mut ops, value, |src| Op::Print { src }),
            doc::Stmt::Finish { value } => self.valued(&mut ops, value, |src| Op::Finish { src }),
            doc::Stmt::Fail { value } => self.valued(&mut ops, value, |src| Op::Fail { src }),
            doc::Stmt::Try(scope) => {
                let mut next = 0;
                let body = self.child(next, |c| c.block(&scope.body, &[]).0);
                next += 1;
                let catch = scope.catch.as_ref().map(|catch| {
                    let (block, bound) = self.child(next, |c| {
                        c.block(&catch.body, std::slice::from_ref(&catch.binding))
                    });
                    next += 1;
                    (Local::unplaced(bound.first().copied().unwrap_or(0)), block)
                });
                let finally = scope
                    .finally
                    .as_ref()
                    .map(|finally| self.child(next, |c| c.block(finally, &[]).0));
                ops.push(Op::Try {
                    body: BlockIx(body.0),
                });
                Form::Try {
                    site: self.site(),
                    body,
                    catch,
                    finally,
                }
            }
        };
        let start = self.stream.ops.len() as u32;
        let len = ops.ops.len() as u32;
        self.stream.ops.append(&mut ops.ops);
        self.spare.push(ops.ops);
        let mut range = OpRange { start, len };
        if let Form::While { test, .. } = &mut form {
            // The `while` op alone starts the statement; its condition is
            // the rest.
            *test = OpRange {
                start: start + 1,
                len: len - 1,
            };
            range.len = 1;
        }
        Stmt {
            ordinal: None,
            ops: range,
            regs: ops.regs,
            form,
        }
    }

    /// A statement's ops, gathered in a buffer an earlier statement left.
    fn ops(&mut self) -> Ops {
        Ops {
            ops: self.spare.pop().unwrap_or_default(),
            regs: 0,
            fuse: self.layout.0.is_multiple_of(2),
        }
    }

    /// A statement whose one expression, child 0, ends in `end`.
    fn valued(&mut self, ops: &mut Ops, value: &doc::Expr, end: fn(Reg) -> Op) -> Form {
        self.child(0, |c| c.expr(ops, value, Reg(0)));
        ops.push(end(Reg(0)));
        Form::Plain
    }

    /// A right-hand side, its value in register 0.
    fn rhs(&mut self, ops: &mut Ops, rhs: &doc::Rhs) -> Form {
        match rhs {
            doc::Rhs::Expr(expr) => {
                self.expr(ops, expr, Reg(0));
                Form::Plain
            }
            doc::Rhs::Action(action) => {
                let action = self.action(action);
                ops.push(Op::Act);
                ops.uses(Reg(0));
                Form::Action(action)
            }
        }
    }

    /// A member's target into `at` and its index into the register after,
    /// its first child having index `first`; gives how it is reached and
    /// the index after its last child.
    fn member(
        &mut self,
        ops: &mut Ops,
        member: &doc::Member,
        first: u32,
        at: Reg,
    ) -> (Option<FieldIx>, u32) {
        match member {
            doc::Member::Field { target, field } => {
                self.child(first, |c| c.expr(ops, target, at));
                (Some(self.field(field)), first + 1)
            }
            doc::Member::Index { target, index } => {
                self.child(first, |c| c.expr(ops, target, at));
                self.child(first + 1, |c| c.expr(ops, index, at.next()));
                (None, first + 2)
            }
        }
    }

    pub(super) fn action(&mut self, action: &doc::Action) -> Action {
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
            doc::Callee::Library(function) => match self.resolve.lib(function) {
                Some(lib) => Callee::Library(lib),
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
            doc::Atom::Literal(literal) => Atom::Literal(literal.clone().into()),
        }
    }

    /// Each expression into the registers from `at` on, in order.
    fn exprs(&mut self, ops: &mut Ops, exprs: &[doc::Expr], at: Reg) -> RegRange {
        for (index, expr) in exprs.iter().enumerate() {
            let reg = Reg(at.0 + index as u32);
            self.child(index as u32, |c| c.expr(ops, expr, reg));
        }
        RegRange {
            start: at,
            len: exprs.len() as u32,
        }
    }

    /// An expression's value into register `dst`, charged before its
    /// sub-expressions.
    fn expr(&mut self, ops: &mut Ops, expr: &doc::Expr, dst: Reg) {
        ops.charge();
        ops.uses(dst);
        match expr {
            doc::Expr::Literal(literal) => {
                let c = self.constant(literal.clone());
                ops.push(Op::Const { dst, c });
            }
            doc::Expr::Variable(name) => {
                let var = self.resolve(name);
                self.read(ops, var, dst);
            }
            doc::Expr::Tuple(items) => {
                let items = self.exprs(ops, items, dst);
                ops.push(Op::MakeTuple { items });
            }
            doc::Expr::List(items) => {
                let items = self.exprs(ops, items, dst);
                ops.push(Op::MakeList { items });
            }
            doc::Expr::Set(items) => {
                let items = self.exprs(ops, items, dst);
                ops.push(Op::MakeSet { items });
            }
            doc::Expr::Map(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    let index = index as u32 * 2;
                    let (key, value) = (Reg(dst.0 + index), Reg(dst.0 + index + 1));
                    self.child(index, |c| c.expr(ops, &entry.key, key));
                    self.child(index + 1, |c| c.expr(ops, &entry.value, value));
                }
                ops.push(Op::MakeMap {
                    entries: RegRange {
                        start: dst,
                        len: entries.len() as u32 * 2,
                    },
                });
            }
            doc::Expr::Record(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    let reg = Reg(dst.0 + index as u32);
                    self.child(index as u32, |c| c.expr(ops, &entry.value, reg));
                }
                let fields = FieldListIx(self.stream.field_lists.len() as u32);
                self.stream
                    .field_lists
                    .push(entries.iter().map(|entry| entry.field.clone()).collect());
                ops.push(Op::MakeRecord {
                    fields,
                    values: RegRange {
                        start: dst,
                        len: entries.len() as u32,
                    },
                });
            }
            doc::Expr::Member(member) => match member.as_ref() {
                doc::Member::Field { target, field } => {
                    self.child(0, |c| c.expr(ops, target, dst));
                    let field = self.field(field);
                    ops.push(Op::Field { at: dst, field });
                }
                doc::Member::Index { target, index } => {
                    self.child(0, |c| c.expr(ops, target, dst));
                    self.child(1, |c| c.expr(ops, index, dst.next()));
                    ops.push(Op::Index { at: dst });
                }
            },
            doc::Expr::Closure(closure) => {
                // A closure runs only where it is applied, which the body of
                // a function with a native implementation cannot do
                // (`K-LIB-004`): wherever it runs, it is ordinary code.
                let code = self.child(0, |c| {
                    c.code(&closure.params, &closure.body, false, true, false)
                });
                let id = CodeId(self.base.codes + self.codes.len() as u32);
                self.codes.push(Some(code));
                ops.push(Op::Closure {
                    dst,
                    code: CodeIx(id.0),
                });
            }
            doc::Expr::Call { function, args } => match self.resolve.lib(function) {
                Some(lib) => {
                    let args = self.exprs(ops, args, dst);
                    ops.push(Op::Call {
                        lib: LibIx(lib.0),
                        args,
                    });
                }
                None => {
                    let name = self.name(Name::new(function.to_string()));
                    ops.push(Op::Unbound { dst, name });
                }
            },
            doc::Expr::Clock => ops.push(Op::Clock { dst }),
            doc::Expr::Random => ops.push(Op::Random { dst }),
            doc::Expr::If { cond, then, r#else } => {
                self.child(0, |c| c.expr(ops, cond, dst));
                let test = ops.ops.len();
                ops.push(Op::JumpIfFalse { cond: dst, skip: 0 });
                self.child(1, |c| c.expr(ops, then, dst));
                let jump = ops.ops.len();
                ops.push(Op::Jump { skip: 0 });
                let otherwise = ops.ops.len();
                self.child(2, |c| c.expr(ops, r#else, dst));
                let end = ops.ops.len();
                ops.ops[test] = Op::JumpIfFalse {
                    cond: dst,
                    skip: (otherwise - test - 1) as u32,
                };
                ops.ops[jump] = Op::Jump {
                    skip: (end - jump - 1) as u32,
                };
            }
            doc::Expr::Let { name, value, body } => {
                self.child(0, |c| c.expr(ops, value, dst));
                if let Some(scopes) = self.scopes.last_mut() {
                    scopes.blocks.push(Vec::new());
                }
                let slot = self.declare(name);
                let local = LocalRef { slot, at: slot };
                ops.push(Op::LetBind { local, src: dst });
                self.child(1, |c| c.expr(ops, body, dst));
                ops.push(Op::LetEnd { local });
                if let Some(scopes) = self.scopes.last_mut() {
                    let declared = scopes.blocks.pop().unwrap_or_default();
                    if let Some(block) = scopes.blocks.last_mut() {
                        block.extend(declared.into_iter().map(|(name, declared)| {
                            (
                                name,
                                match declared {
                                    super::Declared::Slot(slot) => {
                                        super::Declared::ScopedSlot(slot)
                                    }
                                    other => other,
                                },
                            )
                        }));
                    }
                }
            }
            doc::Expr::Read(read) => {
                self.child(0, |c| c.expr(ops, &read.handle, dst));
                self.child(1, |c| c.expr(ops, &read.request, dst.next()));
                ops.push(Op::HostRead { at: dst });
            }
        }
    }

    /// Reads a variable into `dst`.
    fn read(&mut self, ops: &mut Ops, var: Var, dst: Reg) {
        ops.push(match var {
            Var::Local(local) => Op::Local {
                dst,
                local: local_ref(local),
            },
            Var::Session(name) => Op::Session {
                dst,
                name: self.name(name),
            },
            Var::Unbound(name) => Op::Unbound {
                dst,
                name: self.name(name),
            },
        });
    }

    fn constant(&mut self, value: lash_kernel_doc::Literal) -> ConstIx {
        let c = ConstIx(self.stream.consts.len() as u32);
        self.stream.consts.push(value.into());
        c
    }

    fn name(&mut self, name: Name) -> NameIx {
        let names = &mut self.stream.names;
        NameIx(*self.names.entry(name).or_insert_with_key(|name| {
            names.push(name.clone());
            names.len() as u32 - 1
        }))
    }

    fn field(&mut self, field: &str) -> FieldIx {
        if let Some(at) = self.fields.get(field) {
            return FieldIx(*at);
        }
        let at = self.stream.fields.len() as u32;
        self.stream.fields.push(field.to_owned());
        self.fields.insert(field.to_owned(), at);
        FieldIx(at)
    }
}

fn target_of(field: Option<FieldIx>) -> Target {
    field.map_or(Target::Index, Target::Field)
}

/// A variable's slot, placed once its code is complete.
fn local_ref(local: Local) -> LocalRef {
    LocalRef {
        slot: local.slot,
        at: local.at,
    }
}
