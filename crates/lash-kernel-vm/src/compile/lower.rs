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
use lash_kernel_doc::{FunctionId, Name};
use lash_kernel_exec::{
    ActIx, BlockIx, CodeIx, ConstIx, FieldIx, FieldListIx, LibIx, LocalRef, NameIx, Op, OpRange,
    Reg, RegRange,
};

use super::{
    Action, ActionKind, Atom, Callee, CodeId, Compiler, Form, Local, Nested, NestedCallee,
    NestedKind, Runs, Stmt, Var, Via,
};

/// One statement's ops and its static kill set: the dense register interval
/// its expression tree uses, in document evaluation order. This interval is
/// independent of instruction fusion and register write history.
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
        let outer_depth = std::mem::replace(&mut self.stmt_depth, self.path.len() as u32);
        let live = self.live.len();
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
            doc::Stmt::Assign {
                place: doc::Place::Member(member),
                value: doc::Rhs::Expr(value),
                order: doc::AssignOrder::PlaceFirst,
            } => {
                // The target and the index are evaluated first, into
                // registers 0 and 1, and stay there while the right-hand
                // side is evaluated after them: roles of any action nested
                // in it (`K-EVAL-003`).
                let (field, next) = self.member(&mut ops, member, 0, Reg(0));
                let src = Reg(next);
                self.child(next, |c| c.expr(&mut ops, value, src));
                let target = Reg(0);
                ops.push(match target_of(field) {
                    Target::Field(field) => Op::SetField { target, field, src },
                    Target::Index => Op::SetIndex { target, src },
                });
                Form::Plain
            }
            doc::Stmt::Assign { place, value, .. } => match place {
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
                            name: self.name(name.name),
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
                    // (`K-FORM-005`, `K-FORM-006`): a role of any action
                    // nested in them. They compile first, as their
                    // children come first.
                    let value_index = match member {
                        doc::Member::Field { .. } => 1,
                        doc::Member::Index { .. } => 2,
                    };
                    self.live.push((self.stmt_depth, value_index, Reg(0)));
                    let mut written = self.ops();
                    let first_nested = self.stream.nested.len();
                    let (field, next) = self.member(&mut written, member, 0, Reg(1));
                    let place_nested = self.stream.nested.len();
                    self.live.truncate(live);
                    let form = self.child(next, |c| c.rhs(&mut ops, value));
                    ops.push(Op::Pin { src: Reg(0) });
                    ops.regs = ops.regs.max(written.regs);
                    // The place's actions resume in the statement's range.
                    let offset = ops.ops.len() as u32;
                    for nested in &mut self.stream.nested[first_nested..place_nested] {
                        nested.resume += offset;
                    }
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
            doc::Stmt::Do { value } => {
                self.child(0, |c| c.expr(&mut ops, value, Reg(0)));
                ops.push(Op::End);
                Form::Plain
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
                metered,
                binding,
                iterable,
                via,
                body,
                else_block,
            } => {
                self.child(0, |c| c.expr(&mut ops, iterable, Reg(0)));
                let (body, bound) = self.child(1, |c| c.block(body, std::slice::from_ref(binding)));
                let else_block =
                    (!else_block.is_empty()).then(|| self.child(2, |c| c.block(else_block, &[]).0));
                let via = via.as_ref().map(|via| self.via(via));
                if let Some(Err(function)) = via {
                    // Validation refuses a hook it cannot resolve; a
                    // variable of its name is what is left to mean.
                    let name = self.name(Name::new(function.to_string()));
                    ops.push(Op::Unbound { dst: Reg(1), name });
                }
                ops.push(Op::ForIn { iterable: Reg(0) });
                Form::For {
                    metered: *metered,
                    site: self.site(),
                    binding: Local::unplaced(bound.first().copied().unwrap_or(0)),
                    via: via.and_then(Result::ok),
                    body,
                    else_block,
                }
            }
            doc::Stmt::While {
                metered,
                condition,
                body,
                else_block,
            } => {
                // The condition's ops follow the `while`'s own: its test
                // range, which the machine places once the range is known.
                ops.push(Op::While);
                self.child(0, |c| c.expr(&mut ops, condition, Reg(0)));
                ops.push(Op::Test { cond: Reg(0) });
                Form::While {
                    metered: *metered,
                    site: self.site(),
                    test: OpRange { start: 1, len: 0 },
                    body: self.child(1, |c| c.block(body, &[]).0),
                    else_block: (!else_block.is_empty())
                        .then(|| self.child(2, |c| c.block(else_block, &[]).0)),
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
            doc::Stmt::ReturnVia { value, policy } => {
                self.valued(&mut ops, value, |src| Op::ReturnVia { src });
                match self.resolve.lib(policy) {
                    Some(policy) => Form::ReturnVia {
                        site: self.site(),
                        policy,
                    },
                    None => {
                        let name = self.name(Name::new(policy.to_string()));
                        ops.ops.pop();
                        ops.push(Op::Unbound { dst: Reg(0), name });
                        Form::Plain
                    }
                }
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
        self.live.truncate(live);
        self.stmt_depth = outer_depth;
        Stmt {
            ordinal: None,
            ops: range,
            regs: ops.regs,
            form,
        }
    }

    /// A loop protocol's hooks by their library ids, or the first hook the
    /// document cannot reach.
    fn via(&self, via: &doc::Protocol) -> Result<Via, FunctionId> {
        let lib = |function: &FunctionId| self.resolve.lib(function).ok_or(*function);
        let hook = |function: &Option<FunctionId>| function.as_ref().map(lib).transpose();
        Ok(Via {
            fuse_list: self.layout.0.is_multiple_of(4),
            open: lib(&via.open)?,
            next: hook(&via.next)?,
            item: hook(&via.item)?,
            check: hook(&via.check)?,
        })
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
                self.expr_at(ops, first, target, at);
                (Some(self.field(field)), first + 1)
            }
            doc::Member::Index { target, index } => {
                self.expr_at(ops, first, target, at);
                self.expr_at(ops, first + 1, index, at.next());
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
                then_yield,
            } => ActionKind::Perform {
                effect: effect.clone(),
                args: self.atoms(args),
                result: result.clone(),
                then_yield: *then_yield,
            },
            doc::Action::Sleep {
                duration,
                then_yield,
            } => ActionKind::Sleep {
                duration: self.atom(duration),
                then_yield: *then_yield,
            },
            // An await is a call of its policy with the value
            // (`K-TASK-026`).
            doc::Action::Await { policy, value } => ActionKind::Call {
                callee: self.callee(&doc::Callee::Library(*policy)),
                args: vec![self.atom(value)],
            },
            doc::Action::Join { task } => ActionKind::Join(self.atom(task)),
            doc::Action::JoinMany { mode, tasks } => ActionKind::JoinMany(*mode, self.atom(tasks)),
            doc::Action::Yield => ActionKind::Yield,
            doc::Action::Cancel { task } => ActionKind::Cancel(self.atom(task)),
        };
        Action {
            occurrence: usize::MAX,
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
            self.expr_at(ops, index as u32, expr, reg);
        }
        RegRange {
            start: at,
            len: exprs.len() as u32,
        }
    }

    /// Child `index` of the node being compiled, into `reg`: from then on a
    /// role of any action nested after it, until its parent ends.
    fn expr_at(&mut self, ops: &mut Ops, index: u32, expr: &doc::Expr, reg: Reg) {
        let depth = self.path.len() as u32;
        self.child(index, |c| c.expr(ops, expr, reg));
        self.live.push((depth, index, reg));
    }

    /// An action nested in an expression, its arguments in the registers
    /// from `dst` and its value to `dst`: its resume entry, with the roles
    /// standing now, then its op. A library call with a body is always
    /// this op, its native answering first: a native may decline, and the
    /// body's frame then stands at the action (`K-LIB-006`).
    fn nested(
        &mut self,
        ops: &mut Ops,
        kind: NestedKind,
        read: Option<Var>,
        args: &[doc::Expr],
        dst: Reg,
    ) {
        let roles = self
            .live
            .iter()
            .map(|&(depth, index, reg)| {
                let mut path = self.path[..depth as usize].to_vec();
                path.push(index);
                (doc::Site::new(self.unit.clone(), path), reg)
            })
            .collect();
        let site = self.site();
        let args = match read {
            // The callee comes before the arguments (`K-EVAL-004`), a role
            // of the actions in them under the action's own site.
            Some(var) => {
                self.read(ops, var, dst);
                let live = self.live.len();
                let depth = self.path.len() - 1;
                self.live.push((depth as u32, self.path[depth], dst));
                let rest = self.exprs(ops, args, Reg(dst.0 + 1));
                self.live.truncate(live);
                RegRange {
                    start: dst,
                    len: rest.len + 1,
                }
            }
            None => self.exprs(ops, args, dst),
        };
        let act = ActIx(self.stream.nested.len() as u32);
        let op = Op::Nested { act, args };
        self.stream.nested_at.insert(site.clone(), act);
        self.stream.nested.push(Nested {
            occurrence: usize::MAX,
            site,
            depth: self.stmt_depth,
            kind,
            resume: ops.ops.len() as u32 + 1,
            dst,
            roles,
        });
        ops.push(op);
    }

    /// An expression's value into register `dst`, charged before its
    /// sub-expressions.
    fn expr(&mut self, ops: &mut Ops, expr: &doc::Expr, dst: Reg) {
        ops.charge();
        ops.uses(dst);
        let live = self.live.len();
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
                    self.expr_at(ops, index, &entry.key, key);
                    self.expr_at(ops, index + 1, &entry.value, value);
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
                    self.expr_at(ops, index as u32, &entry.value, reg);
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
                    self.expr_at(ops, 0, target, dst);
                    let field = self.field(field);
                    ops.push(Op::Field { at: dst, field });
                }
                doc::Member::Index { target, index } => {
                    self.expr_at(ops, 0, target, dst);
                    self.expr_at(ops, 1, index, dst.next());
                    ops.push(Op::Index { at: dst });
                }
            },
            doc::Expr::Closure(closure) => {
                // A closure runs only where it is applied, in the body that
                // made it or wherever it escapes to: wherever it runs, it is
                // ordinary code, charged as it runs (`K-CHG-007`).
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
                // A function that runs its body enters a frame: the
                // statement stands at the call (`K-EVAL-008`).
                Some(lib) if self.resolve.runs(lib) != Runs::Answers => {
                    self.nested(ops, NestedKind::Body(lib), None, args, dst);
                }
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
                self.expr_at(ops, 0, &read.handle, dst);
                self.expr_at(ops, 1, &read.request, dst.next());
                ops.push(Op::HostRead { at: dst });
            }
            doc::Expr::Act(act) => {
                let mut read = None;
                let mut runs = |callee: &doc::Callee| match self.callee(callee) {
                    Callee::Declared(code) => NestedCallee::Declared(code),
                    Callee::Library(lib) => NestedCallee::Library(lib),
                    Callee::Value(var) => {
                        read = Some(var);
                        NestedCallee::Read
                    }
                };
                let kind = match &act.kind {
                    doc::ActKind::Call(callee) => NestedKind::Call(runs(callee)),
                    doc::ActKind::Perform {
                        effect,
                        result,
                        then_yield,
                    } => NestedKind::Perform {
                        effect: effect.clone(),
                        result: result.clone(),
                        then_yield: *then_yield,
                    },
                    doc::ActKind::Sleep { then_yield } => NestedKind::Sleep {
                        then_yield: *then_yield,
                    },
                    doc::ActKind::Spawn(callee) => NestedKind::Spawn(runs(callee)),
                    doc::ActKind::Yield => NestedKind::Yield,
                    doc::ActKind::Cancel => NestedKind::Cancel,
                    doc::ActKind::Join => NestedKind::Join,
                    doc::ActKind::JoinMany(mode) => NestedKind::JoinMany(*mode),
                    // An await is a call of its policy with the value
                    // (`K-TASK-026`).
                    doc::ActKind::Await(policy) => {
                        NestedKind::Call(runs(&doc::Callee::Library(*policy)))
                    }
                };
                self.nested(ops, kind, read, &act.args, dst);
            }
        }
        self.live.truncate(live);
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
                name: self.name(name.name),
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
            names.push(super::BindingName::new(name.clone()));
            names.len() as u32 - 1
        }))
    }

    fn field(&mut self, field: &str) -> FieldIx {
        let at = self.stream.fields.len() as u32;
        self.stream
            .fields
            .push(super::Field::new(field, self.layout));
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
