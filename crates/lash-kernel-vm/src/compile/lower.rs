//! Lowering: each statement of a document or a library body to its range
//! of the flat stream (`lash_kernel_exec::Op`).
//!
//! A statement's expressions lower in evaluation order, each node's value
//! to the register its depth names, its one-unit charge event at the
//! node's pre-order position (`K-CHG-002`): a node is charged before its
//! sub-expressions are evaluated, so a bound crossed at the node ends the
//! run before anything under it can raise. No op charges a fixed event:
//! each straight-line run of them is a charge region, headed by an
//! `Op::Region` and stated in the stream's side table, which the machine
//! admits whole when it cannot pass the bound and replays as the reference
//! charges otherwise. Where regions split is a function of the layout, so
//! the laws compare splits. Children compile in their child order, so
//! closures take their codes in the order the document's sites give them.
//!
//! Each statement's ops are gathered apart and appended whole when the
//! statement is done, after the statements of the blocks it holds, so
//! every statement is one contiguous range. Its variables are placed once
//! its code is complete (`super::Placer`).

use lash_kernel_doc as doc;
use lash_kernel_exec::charge::RegionEvent;
use lash_kernel_exec::{
    ActIx, BlockIx, ChargeEvent, CodeIx, ConstIx, FieldIx, FieldListIx, LibIx, LocalRef, NameIx,
    Op, OpRange, Reg, RegRange, RegionIx, RegionTable, SiteIx, Weight,
};
use lash_vocabulary::names::FunctionId;
use lash_vocabulary::names::Name;

use super::{
    Action, ActionKind, Atom, Callee, CodeId, Compiler, Form, Local, Nested, NestedCallee,
    NestedKind, NestedRole, Runs, Shuffler, Stmt, Var, Via,
};
use crate::functions::brand_built;

/// One statement's ops, the charge regions they head, and its static
/// kill set: the dense register interval its expression tree uses, in
/// document evaluation order. This interval is independent of instruction
/// fusion and register write history.
struct Ops {
    metered: bool,
    ops: Vec<Op>,
    regs: u32,
    regions: Regions,
    /// Whether the last region is open: the ops since its head stand in it.
    open: bool,
    /// Chooses, for a layout other than 0, where a region splits.
    split: Option<Shuffler>,
}

/// The charge regions of a statement's ops as they are gathered: each
/// region's head and first event, and each event with the position of the
/// op it is charged before. Emptied ones are reused.
#[derive(Default)]
pub(super) struct Regions {
    heads: Vec<(u32, u32)>,
    at: Vec<u32>,
    events: Vec<RegionEvent>,
}

impl Regions {
    fn clear(&mut self) {
        self.heads.clear();
        self.at.clear();
        self.events.clear();
    }
}

impl Ops {
    /// Pushes `op`; one that stands in no region closes the open one.
    fn push(&mut self, op: Op) {
        if !op.kind().in_region() {
            self.open = false;
        }
        self.ops.push(op);
    }

    /// Ends the open region: what follows is a jump's target.
    fn close(&mut self) {
        self.open = false;
    }

    /// A fixed charge event before the next op, in the open region or one
    /// headed here; adjacent one-unit events at one op join.
    fn event(&mut self, event: ChargeEvent, weight: Weight) {
        if !self.metered {
            return;
        }
        if self.open
            && let Some(split) = &mut self.split
            && split.next() % 3 == 0
        {
            self.open = false;
        }
        let regions = &mut self.regions;
        if !self.open {
            regions
                .heads
                .push((self.ops.len() as u32, regions.at.len() as u32));
            self.ops.push(Op::Region {
                region: RegionIx(u32::MAX),
                total: 0,
            });
            self.open = true;
        }
        let at = self.ops.len() as u32;
        let first = regions.heads.last().map_or(0, |(_, first)| *first as usize);
        if regions.at.len() > first
            && regions.at.last() == Some(&at)
            && let Some(previous) = regions.events.last_mut()
            && previous.event == event
            && let (Weight::Units(units), Weight::Units(more)) = (&mut previous.weight, weight)
        {
            *units += more;
            return;
        }
        regions.at.push(at);
        regions.events.push(RegionEvent { event, weight });
    }

    /// One more one-unit charge event: an expression node's.
    fn charge(&mut self) {
        self.event(ChargeEvent::ExprNode, Weight::Units(1));
    }

    /// A construction's members, charged whole before it allocates.
    fn construction(&mut self, members: u32) {
        if members > 0 {
            self.event(
                ChargeEvent::Construction,
                Weight::Atomic(u64::from(members)),
            );
        }
    }

    /// Appends `other`'s ops, which run right after these, and the regions
    /// they head; gives back its emptied buffers.
    fn append(&mut self, mut other: Ops) -> (Vec<Op>, Regions) {
        let offset = self.ops.len() as u32;
        let events = self.regions.at.len() as u32;
        let regions = &mut self.regions;
        regions.heads.extend(
            other
                .regions
                .heads
                .iter()
                .map(|(head, first)| (head + offset, first + events)),
        );
        regions
            .at
            .extend(other.regions.at.iter().map(|at| at + offset));
        regions.events.append(&mut other.regions.events);
        self.open = other.open;
        self.regs = self.regs.max(other.regs);
        self.ops.append(&mut other.ops);
        other.regions.clear();
        (other.ops, other.regions)
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
                    // The place's actions resume in the statement's range.
                    let offset = ops.ops.len() as u32;
                    for nested in &mut self.stream.nested[first_nested..place_nested] {
                        nested.resume += offset;
                    }
                    let (spare, regions) = ops.append(written);
                    self.spare.push(spare);
                    self.spare_regions.push(regions);
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
                    metered: if self.metered { *metered } else { 0 },
                    site: self.site(),
                    binding: Local::unplaced(bound.first().copied().unwrap_or(0)),
                    via: via.and_then(Result::ok),
                    body,
                    else_block,
                    region: None,
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
                    metered: if self.metered { *metered } else { 0 },
                    site: self.site(),
                    test: OpRange { start: 1, len: 0 },
                    body: self.child(1, |c| c.block(body, &[]).0),
                    else_block: (!else_block.is_empty())
                        .then(|| self.child(2, |c| c.block(else_block, &[]).0)),
                    region: None,
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
                match self.hook(policy).ok() {
                    Some(policy) => Form::ReturnVia {
                        site: self.site(),
                        policy,
                    },
                    None => {
                        let name = self.name(Name::new(format!("{policy:?}")));
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
        place_regions(&mut ops, &mut self.stream.regions, start);
        self.stream.ops.append(&mut ops.ops);
        self.spare.push(ops.ops);
        ops.regions.clear();
        self.spare_regions.push(ops.regions);
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
    fn hook(&self, function: &doc::FunctionTarget) -> Result<super::LinkedHook, FunctionId> {
        match function {
            doc::FunctionTarget::Import(slot) => Ok(super::LinkedHook::Import(*slot, None)),
            doc::FunctionTarget::Local(id) => self
                .declared
                .get(id)
                .copied()
                .map(super::LinkedHook::Code)
                .or_else(|| self.resolve.lib(id).map(super::LinkedHook::Library))
                .ok_or(*id),
        }
    }
    fn via(&self, via: &doc::Protocol) -> Result<Via, FunctionId> {
        use lash_vocabulary::Type;
        // The cursor supplies these kinds before the public call boundary.
        let hinted =
            |target: &doc::FunctionTarget, kinds: &[Type], result: Type| {
                let mut hook = self.hook(target)?;
                if let super::LinkedHook::Import(slot, hint) = &mut hook {
                    *hint =
                        self.imports
                            .get(&(self.unit.clone(), *slot))
                            .copied()
                            .filter(|id| {
                                let signature = &self.resolve.library.lib(*id).header.signature;
                                signature.params.len() == kinds.len()
                                    && signature.params.iter().zip(kinds).all(|(param, kind)| {
                                        param.ty == Type::Any || &param.ty == kind
                                    })
                                    && (signature.result == Type::Any || signature.result == result)
                            });
                }
                Ok(hook)
            };
        Ok(Via {
            open: self.hook(&via.open)?,
            next: via.next.as_ref().map(|f| self.hook(f)).transpose()?,
            item: via
                .item
                .as_ref()
                .map(|f| hinted(f, &[Type::Any, Type::Any], Type::Any))
                .transpose()?,
            check: via
                .check
                .as_ref()
                .map(|f| hinted(f, &[Type::Any, Type::Int, Type::Int, Type::Bool], Type::Int))
                .transpose()?,
        })
    }

    /// A statement's ops, gathered in a buffer an earlier statement left.
    fn ops(&mut self) -> Ops {
        let layout = self.layout.0;
        Ops {
            metered: self.metered,
            ops: self.spare.pop().unwrap_or_default(),
            regs: 0,
            regions: self.spare_regions.pop().unwrap_or_default(),
            open: false,
            split: (layout != 0).then_some(Shuffler(layout ^ self.stream.ops.len() as u64)),
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
                slot,
                args,
                result,
                then_yield,
            } => ActionKind::Perform {
                slot: *slot,
                args: self.atoms(args),
                result: result.clone(),
                then_yield: *then_yield,
            },
            // An await is a call of its policy with the value
            // (`K-TASK-026`).
            doc::Action::Await { policy, value } => ActionKind::Call {
                callee: self.callee(&match policy {
                    doc::FunctionTarget::Local(id) => doc::Callee::Local(*id),
                    doc::FunctionTarget::Import(slot) => doc::Callee::Import(*slot),
                }),
                args: vec![self.atom(value)],
            },
            doc::Action::Join { task } => ActionKind::Join(self.atom(task)),
            doc::Action::JoinMany { mode, tasks } => ActionKind::JoinMany(*mode, self.atom(tasks)),
            doc::Action::Yield => ActionKind::Yield,
            doc::Action::Cancel { task } => ActionKind::Cancel(self.atom(task)),
        };
        Action {
            guest: self.scopes.last().is_some_and(|scope| scope.guest),
            occurrence: usize::MAX,
            site: self.site(),
            kind,
        }
    }

    fn callee(&mut self, callee: &doc::Callee) -> Callee {
        match callee {
            doc::Callee::Local(function) => match self.declared.get(function) {
                Some(code) => Callee::Declared(*code),
                None => match self.resolve.lib(function) {
                    Some(lib) => {
                        let header = &self.resolve.library.lib(lib).header;
                        let checked = self.scopes.last().is_some_and(|scope| scope.charged)
                            && !header.has_native()
                            && self
                                .resolve
                                .helpers
                                .is_some_and(|helpers| helpers.contains(function))
                            && (header.signature.result != lash_vocabulary::Type::Any
                                || header
                                    .signature
                                    .params
                                    .iter()
                                    .any(|param| param.ty != lash_vocabulary::Type::Any));
                        if checked {
                            Callee::CheckedHelper(lib)
                        } else {
                            Callee::Library(lib)
                        }
                    }
                    None => Callee::Value(Var::Unbound(Name::new(function.to_string()))),
                },
            },
            doc::Callee::Import(slot) => Callee::Import(
                *slot,
                self.imports
                    .get(&(self.unit.clone(), *slot))
                    .copied()
                    .filter(|id| {
                        self.resolve
                            .library
                            .lib(*id)
                            .header
                            .signature
                            .params
                            .iter()
                            .all(|param| param.ty == lash_vocabulary::Type::Any)
                    }),
            ),
            doc::Callee::Value(name) => Callee::Value(self.resolve(name)),
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
        let statement = self.stmt_depth as usize;
        let roles = self
            .live
            .iter()
            .map(|&(depth, index, reg)| {
                let mut path = self.path[..depth as usize].to_vec();
                path.push(index);
                NestedRole {
                    suffix: path[statement..].into(),
                    site: lash_vocabulary::sites::Site::new(self.unit.clone(), path),
                    reg,
                }
            })
            .collect();
        let site = self.site();
        let suffix = site.path[statement..].into();
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
        let operator = kind
            .operator_lib()
            .is_some_and(|lib| self.resolve.operator(lib, args.len as usize));
        self.stream.nested.push(Nested {
            guest: self.scopes.last().is_some_and(|scope| scope.guest),
            occurrence: usize::MAX,
            site,
            suffix,
            depth: self.stmt_depth,
            kind,
            operator,
            island: false,
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
                ops.construction(items.len);
                ops.push(Op::MakeTuple { items });
            }
            doc::Expr::List(items) => {
                let items = self.exprs(ops, items, dst);
                ops.construction(items.len);
                ops.push(Op::MakeList { items });
            }
            doc::Expr::Set(items) => {
                let items = self.exprs(ops, items, dst);
                ops.construction(items.len);
                ops.push(Op::MakeSet { items });
            }
            doc::Expr::Map(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    let index = index as u32 * 2;
                    let (key, value) = (Reg(dst.0 + index), Reg(dst.0 + index + 1));
                    self.expr_at(ops, index, &entry.key, key);
                    self.expr_at(ops, index + 1, &entry.value, value);
                }
                ops.construction(entries.len() as u32);
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
                ops.construction(entries.len() as u32);
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
                    let site = self.read_site();
                    ops.push(Op::Index { at: dst, site });
                }
            },
            doc::Expr::Closure(closure) => {
                // A closure runs only where it is applied, in the body that
                // made it or wherever it escapes to: wherever it runs, it is
                // ordinary code, charged as it runs (`K-CHG-007`).
                let Some(registered) = self.resolve.library.registry.get(&closure.function) else {
                    // Restored selection can exclude a completed creation site.
                    // Keep its original code position without requiring dead code.
                    let name = self.name(Name::new(closure.function.to_string()));
                    ops.push(Op::Unbound { dst, name });
                    return;
                };
                let definition = registered.definition();
                let params: Vec<_> = definition
                    .signature
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .collect();
                let body = &definition
                    .body()
                    .unwrap_or_else(|| unreachable!("closure body"))
                    .block;
                let enclosing_unit = std::mem::replace(
                    &mut self.unit,
                    lash_vocabulary::sites::Unit::Library(closure.function),
                );
                let enclosing_path = std::mem::take(&mut self.path);
                let mut code = self.code(&params, body, false, true, false);
                code.created = Some(lash_vocabulary::sites::Site::new(
                    enclosing_unit.clone(),
                    enclosing_path.clone(),
                ));
                self.unit = enclosing_unit;
                self.path = enclosing_path;
                let id = CodeId(self.base.codes + self.codes.len() as u32);
                self.codes.push(Some(code));
                ops.push(Op::Closure {
                    dst,
                    code: CodeIx(id.0),
                });
            }
            // `brand.built()` answers the library body it is written in
            // (`K-LIB-010`): a constant of that body, as a literal is.
            doc::Expr::Call { function, .. }
                if *function == brand_built() && self.resolve.lib(function).is_some() =>
            {
                let c = self.constant(match &self.unit {
                    lash_vocabulary::sites::Unit::Library(body) => {
                        doc::Literal::Bytes(doc::Bytes::new(&body.as_bytes()[..]))
                    }
                    lash_vocabulary::sites::Unit::Main
                    | lash_vocabulary::sites::Unit::Function(_) => doc::Literal::Absent,
                });
                ops.push(Op::Const { dst, c });
            }
            doc::Expr::Call { function, args } => match self.resolve.lib(function) {
                // A function that runs its body enters a frame: the
                // statement stands at the call (`K-EVAL-008`).
                Some(lib) if self.resolve.runs(lib) != Runs::Answers => {
                    self.nested(ops, NestedKind::Body(lib), None, args, dst);
                }
                Some(lib) => {
                    let args = self.exprs(ops, args, dst);
                    let lib_ix = LibIx(lib.0);
                    ops.push(if self.resolve.operator(lib, args.len as usize) {
                        Op::Operator { lib: lib_ix, args }
                    } else {
                        Op::Call { lib: lib_ix, args }
                    });
                }
                None => {
                    let name = self.name(Name::new(function.to_string()));
                    ops.push(Op::Unbound { dst, name });
                }
            },

            doc::Expr::If { cond, then, r#else } => {
                self.child(0, |c| c.expr(ops, cond, dst));
                let test = ops.ops.len();
                ops.push(Op::JumpIfFalse { cond: dst, skip: 0 });
                self.child(1, |c| c.expr(ops, then, dst));
                let jump = ops.ops.len();
                ops.push(Op::Jump { skip: 0 });
                let otherwise = ops.ops.len();
                self.child(2, |c| c.expr(ops, r#else, dst));
                // Both arms reach what follows: a region of its own.
                ops.close();
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
            doc::Expr::Act(act) => {
                let mut read = None;
                let mut runs = |callee: &doc::Callee| match self.callee(callee) {
                    Callee::Declared(code) => NestedCallee::Declared(code),
                    Callee::Library(lib) => NestedCallee::Library(lib),
                    Callee::CheckedHelper(lib) => NestedCallee::CheckedHelper(lib),
                    Callee::Import(slot, lib) => NestedCallee::Import(slot, lib),
                    Callee::Value(var) => {
                        read = Some(var);
                        NestedCallee::Read
                    }
                };
                let kind = match &act.kind {
                    doc::ActKind::Call(callee) => NestedKind::Call(runs(callee)),
                    doc::ActKind::Perform {
                        slot,
                        result,
                        then_yield,
                    } => NestedKind::Perform {
                        slot: *slot,
                        result: result.clone(),
                        then_yield: *then_yield,
                    },
                    doc::ActKind::Spawn(callee) => NestedKind::Spawn(runs(callee)),
                    doc::ActKind::Yield => NestedKind::Yield,
                    doc::ActKind::Cancel => NestedKind::Cancel,
                    doc::ActKind::Join => NestedKind::Join,
                    doc::ActKind::JoinMany(mode) => NestedKind::JoinMany(*mode),
                    // An await is a call of its policy with the value
                    // (`K-TASK-026`).
                    doc::ActKind::Await(policy) => NestedKind::Call(runs(&match policy {
                        doc::FunctionTarget::Local(id) => doc::Callee::Local(*id),
                        doc::FunctionTarget::Import(slot) => doc::Callee::Import(*slot),
                    })),
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
        let site = self.read_site();
        self.stream
            .fields
            .push(super::Field::new(field, site.0, self.layout));
        FieldIx(at)
    }

    /// A new property read site.
    fn read_site(&mut self) -> SiteIx {
        let site = SiteIx(self.sites);
        self.sites += 1;
        site
    }
}

/// Adds the regions `ops` heads to the stream's `table`, its ops starting
/// at `start`, and states each in its head.
#[expect(
    clippy::expect_used,
    reason = "lowering states each region's fixed events after its head, in order"
)]
fn place_regions(ops: &mut Ops, table: &mut RegionTable, start: u32) {
    let regions = &ops.regions;
    for (index, (head, first)) in regions.heads.iter().enumerate() {
        let end = regions
            .heads
            .get(index + 1)
            .map_or(regions.events.len(), |(_, next)| *next as usize);
        let first = *first as usize;
        let (region, total) = table
            .push(
                start + head,
                regions.at[first..end].iter().map(|at| start + at),
                &regions.events[first..end],
            )
            .expect("lowering states a fixed region");
        ops.ops[*head as usize] = Op::Region {
            region: RegionIx(region),
            total: u32::try_from(total).unwrap_or(u32::MAX),
        };
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
