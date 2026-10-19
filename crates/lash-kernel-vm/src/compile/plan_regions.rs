//! Loop regions of generated helper plans (FIG-6780): a `for` of a helper
//! body that applies a callback, run by its plan as a region over the
//! loop's own cursor.
//!
//! A segment's plan walks a list loop element by element through its
//! generated code: each statement a dispatch, each callback an entry
//! through the plan's site. A [`PlanRegion`] states the same loop as its
//! body's resolved statements ([`super::expr`]) over the plan's own
//! variables and registers, which the machine runs whole iterations of
//! from the plan's loop step, entering the callback's island inline. It
//! performs the plan's forms in the plan's order on the plan's registers
//! and variables, so where it stops (between iterations, at a callback
//! site, or at a raise) the plan stands exactly where its generated code
//! would; a callback that does not return there stands its frames as the
//! segment path materialises them.
//!
//! A loop forms when it is a `for` with no `else` and no protocol, outside
//! any `try`, whose body statements are `if`, `break`, `let` and `set` of
//! the plan's unshared variables, member writes and `do`, of expressions
//! that read variables and resident constants, call library functions the
//! plan calls, and build the empty tuple; a statement's root action is a
//! callback the plan applies at its site, or a library call. A call the
//! plan invokes at a site, as a statement's own action or nested in its
//! expression, is offered there as the plan offers it, so a callee that
//! runs its body stands the plan at that site (FIG-6797). Plans belong
//! to the layout's prepared library: deterministic, immutable and never
//! saved, formed only when the layout forms regions.
//!
//! A region's expressions are fused (FIG-6847): a variable or a constant
//! operand is read in place, a call its prepared entry answers directly
//! carries that entry, and each statement names the plan registers it
//! uses, which its end empties.

use lash_kernel_doc::Value;
use lash_kernel_doc::plan::{HelperPlan, SiteCall};
use lash_kernel_exec::{LocalRef, Op, Reg};

use super::expr::{self, Expr, Kind, Make};
use super::segments::{Segment, SiteControl};
use super::{
    ActionKind, Atom, Base, BlockId, Callee, Constant, Entry, Form, Lib, LibId, Local, StmtId,
    Tables, Var,
};

/// A `for` of a helper plan that runs as a region over its cursor.
pub(crate) struct PlanRegion {
    /// The plan variable that holds what the loop walks: the loop the
    /// plan opens with it is this one.
    pub(crate) walked: u32,
    /// The plan variable each element is bound to.
    pub(crate) binding: u32,
    /// The units each test of the loop charges.
    pub(crate) metered: u64,
    pub(crate) body: Block,
}

/// A block of the loop's body, its statements in order.
pub(crate) struct Block {
    pub(crate) steps: Box<[Step]>,
    /// The plan variables it declares, which its end empties.
    pub(crate) declares: Box<[u32]>,
}

/// One statement: each opens a statement of the plan, which puts its
/// operands in the plan registers below `span` where its forms answer as
/// fused; a form that does not widens the statement to all of them.
pub(crate) struct Step {
    pub(crate) span: u32,
    pub(crate) kind: StepKind,
}

pub(crate) enum StepKind {
    If {
        cond: Node,
        then: Block,
        otherwise: Block,
    },
    /// A `break`: it empties these variables, the blocks' it leaves.
    Break(Box<[u32]>),
    /// `set` of an unshared plan variable.
    Assign { slot: u32, value: Node },
    /// `let` of an unshared plan variable.
    Bind { slot: u32, value: Node },
    /// `set target[index] = value`: the three in the plan's evaluation
    /// order, the target in register `target` before the index, the value
    /// in register `src`. Each goes to its register as it is evaluated
    /// when `placed`, as one after the first stands the plan at a site.
    Store {
        order: [Node; 3],
        target: u32,
        src: u32,
        placed: bool,
    },
    /// `do`: a value computed and dropped.
    Discard(Node),
}

/// A value a statement computes into its register.
pub(crate) struct Node {
    pub(crate) reg: u32,
    pub(crate) kind: NodeKind,
}

pub(crate) enum NodeKind {
    /// A plan variable's value.
    Local(u32),
    /// A resident constant.
    Const(Value),
    /// The value held while the place after it is evaluated.
    Pin(Box<Node>),
    /// The empty tuple.
    Unit,
    /// An `if`'s test `same(value, ())` by `lib`, the kernel's `same`, on
    /// its two arguments: whether the value is the empty tuple, which the
    /// statement need not build.
    IsUnit { lib: LibId, args: Box<[Node]> },
    /// A call of library function `lib` on the arguments from the node's
    /// register, which the plan answers by its general call. Where that
    /// call takes a prepared entry first (a call of every parameter, no
    /// operation, at the plan's own depth) and no operand stands the plan
    /// at a site, the call is fused: `direct` is that entry, handed the
    /// operands' values in place, and the value it answers goes to no
    /// register.
    Call {
        lib: LibId,
        args: Box<[Node]>,
        direct: Option<Entry>,
    },
    /// `list.len` by `lib` of the list `list` gives, which the plan
    /// answers by its general call: a resident list's length is read from
    /// the arena ([`Lib::length_read`]), with that call's verdict points.
    Length { lib: LibId, list: Box<Node> },
    /// The statement's root action: the callback in plan variable
    /// `callee`, applied at site `site` to the arguments after it. The
    /// region writes that variable nowhere when `steady`: it holds the
    /// same callback for the whole run of the loop.
    Apply {
        site: u32,
        callee: u32,
        args: Box<[Node]>,
        steady: bool,
    },
    /// A call of `lib` its plan offers at site `site`, which may run its
    /// body in a frame: the statement's root action, or a call nested in
    /// its expression that the plan invokes there.
    Invoke {
        site: u32,
        lib: LibId,
        args: Box<[Node]>,
    },
}

/// The most operands a fused call holds in place.
pub(crate) const IN_PLACE: usize = 4;

impl Node {
    /// The plan registers below which this node and its operands put
    /// what they compute, as the plan's code puts it.
    fn span(&self) -> u32 {
        let args = match &self.kind {
            NodeKind::Local(_) | NodeKind::Const(_) | NodeKind::Unit => return self.reg + 1,
            NodeKind::Pin(value) | NodeKind::Length { list: value, .. } => {
                return value.span().max(self.reg + 1);
            }
            NodeKind::IsUnit { args, .. }
            | NodeKind::Call { args, .. }
            | NodeKind::Apply { args, .. }
            | NodeKind::Invoke { args, .. } => args,
        };
        args.iter().map(Node::span).fold(self.reg + 1, u32::max)
    }

    /// The plan registers below which this node puts what it computes
    /// where each form answers as fused: a variable, a constant, a fused
    /// call and a hole test put nothing of their own.
    fn fused(&self) -> u32 {
        match &self.kind {
            NodeKind::Local(_) | NodeKind::Const(_) => 0,
            // The tuple it compares with is built only where it widens.
            NodeKind::IsUnit { args, .. } => args.first().map_or(0, Node::fused),
            NodeKind::Pin(value) | NodeKind::Length { list: value, .. } => value.fused(),
            NodeKind::Call {
                args,
                direct: Some(_),
                ..
            } => args.iter().map(Node::fused).max().unwrap_or(0),
            NodeKind::Unit
            | NodeKind::Call { direct: None, .. }
            | NodeKind::Apply { .. }
            | NodeKind::Invoke { .. } => self.span(),
        }
    }

    /// Whether this node or one of its operands is offered at a site,
    /// where the plan may stand.
    fn stands(&self) -> bool {
        match &self.kind {
            NodeKind::Local(_) | NodeKind::Const(_) | NodeKind::Unit => false,
            NodeKind::Apply { .. } | NodeKind::Invoke { .. } => true,
            NodeKind::Pin(value) | NodeKind::Length { list: value, .. } => value.stands(),
            NodeKind::IsUnit { args, .. } | NodeKind::Call { args, .. } => {
                args.iter().any(Node::stands)
            }
        }
    }
}

impl Block {
    /// Adds the plan variables the block's statements write to `written`.
    fn written(&self, written: &mut Vec<u32>) {
        for step in &self.steps {
            match &step.kind {
                StepKind::If {
                    then, otherwise, ..
                } => {
                    then.written(written);
                    otherwise.written(written);
                }
                StepKind::Assign { slot, .. } | StepKind::Bind { slot, .. } => written.push(*slot),
                StepKind::Break(_) | StepKind::Store { .. } | StepKind::Discard(_) => {}
            }
        }
    }

    /// Marks steady each callback applied from a variable not in
    /// `written`.
    fn settle(&mut self, written: &[u32]) {
        for step in &mut self.steps {
            match &mut step.kind {
                StepKind::If {
                    cond,
                    then,
                    otherwise,
                } => {
                    cond.settle(written);
                    then.settle(written);
                    otherwise.settle(written);
                }
                StepKind::Assign { value, .. }
                | StepKind::Bind { value, .. }
                | StepKind::Discard(value) => value.settle(written),
                StepKind::Store { order, .. } => {
                    for node in order {
                        node.settle(written);
                    }
                }
                StepKind::Break(_) => {}
            }
        }
    }
}

impl Node {
    /// Marks steady each callback this node applies from a variable not in
    /// `written`.
    fn settle(&mut self, written: &[u32]) {
        match &mut self.kind {
            NodeKind::Local(_) | NodeKind::Const(_) | NodeKind::Unit => {}
            NodeKind::Pin(value) | NodeKind::Length { list: value, .. } => value.settle(written),
            NodeKind::Apply {
                callee,
                args,
                steady,
                ..
            } => {
                *steady = !written.contains(callee);
                for arg in args {
                    arg.settle(written);
                }
            }
            NodeKind::IsUnit { args, .. }
            | NodeKind::Call { args, .. }
            | NodeKind::Invoke { args, .. } => {
                for arg in args {
                    arg.settle(written);
                }
            }
        }
    }
}

impl StepKind {
    /// The plan registers the statement puts operands in where each of its
    /// forms answers as fused: a list write writes from the values in
    /// place.
    fn fused(&self) -> u32 {
        match self {
            Self::If { cond, .. } => cond.fused(),
            Self::Break(_) => 0,
            Self::Assign { value, .. } | Self::Bind { value, .. } | Self::Discard(value) => {
                value.fused()
            }
            Self::Store {
                order,
                target,
                src,
                placed: true,
            } => order
                .iter()
                .map(Node::span)
                .fold((target + 2).max(src + 1), u32::max),
            Self::Store { order, .. } => order.iter().map(Node::fused).max().unwrap_or(0),
        }
    }
}

impl From<StepKind> for Step {
    fn from(kind: StepKind) -> Self {
        Self {
            span: kind.fused(),
            kind,
        }
    }
}

/// What forming one plan's regions reads.
struct Former<'a, 'l> {
    plan: &'static HelperPlan,
    locals: &'a [Option<Local>],
    segment: &'a Segment,
    tables: &'a Tables,
    base: &'a Base,
    /// The library's functions, by their table indexes.
    lib: &'a dyn Fn(LibId) -> Option<&'l Lib>,
    /// The plan registers its statements may use.
    registers: u32,
}

/// Forms the regions of the loops `plan`'s callback sites stand in.
pub(super) fn form<'l>(
    plan: &'static HelperPlan,
    locals: &[Option<Local>],
    segment: &Segment,
    tables: &Tables,
    base: &Base,
    lib: &dyn Fn(LibId) -> Option<&'l Lib>,
) -> Vec<PlanRegion> {
    let former = Former {
        plan,
        locals,
        segment,
        tables,
        base,
        lib,
        registers: plan.registers,
    };
    let mut loops: Vec<(StmtId, u32)> = Vec::new();
    for site in &segment.sites {
        // A loop inside a `try` stands in its handler's protection, which
        // a region does not hold.
        let mut guarded = false;
        for control in &site.control {
            match *control {
                SiteControl::Try { .. } => guarded = true,
                SiteControl::For { stmt, walked }
                    if !guarded && !loops.contains(&(stmt, walked)) =>
                {
                    loops.push((stmt, walked));
                }
                _ => {}
            }
        }
    }
    loops
        .into_iter()
        .filter_map(|(stmt, walked)| former.region(stmt, walked))
        .collect()
}

impl Former<'_, '_> {
    fn stmt(&self, id: StmtId) -> &super::Stmt {
        &self.tables.stmts[(id.0 - self.base.stmts) as usize]
    }

    fn block_of(&self, id: BlockId) -> &super::Block {
        &self.tables.blocks[(id.0 - self.base.blocks) as usize]
    }

    /// The plan variable of the body's code slot `slot`, when no closure
    /// shares it.
    fn slot(&self, slot: u32) -> Option<u32> {
        let at = self
            .locals
            .iter()
            .position(|local| local.is_some_and(|local| local.slot == slot))?;
        let local = self.locals[at]?;
        (!local.shared).then_some(at as u32)
    }

    fn local(&self, local: LocalRef) -> Option<u32> {
        self.slot(local.slot)
    }

    fn region(&self, stmt: StmtId, walked: u32) -> Option<PlanRegion> {
        let Form::For {
            metered,
            binding,
            via: None,
            body,
            else_block: None,
            ..
        } = &self.stmt(stmt).form
        else {
            return None;
        };
        let binding = self.slot(binding.slot)?;
        let mut body = self.block(*body, &mut Vec::new())?;
        let mut written = vec![binding];
        body.written(&mut written);
        body.settle(&written);
        Some(PlanRegion {
            walked,
            binding,
            metered: *metered,
            body,
        })
    }

    /// The plan variables block `id` declares: each named one whose
    /// declaration is the block itself or one of its statements.
    fn declares(&self, id: BlockId) -> Option<Box<[u32]>> {
        let block = self.block_of(id);
        let path = &block.site.path;
        let declared: Box<[u32]> = self
            .plan
            .slots
            .iter()
            .enumerate()
            .filter(|(_, local)| {
                !local.name.is_empty()
                    && local.path.starts_with(path)
                    && local.path.len() <= path.len() + 1
            })
            .map(|(at, _)| at as u32)
            .collect();
        (declared.len() == block.declares.len()).then_some(declared)
    }

    /// Block `id`, inside the blocks whose declarations `enclosing` holds,
    /// innermost last.
    fn block(&self, id: BlockId, enclosing: &mut Vec<Box<[u32]>>) -> Option<Block> {
        let declares = self.declares(id)?;
        enclosing.push(declares.clone());
        let steps = self
            .block_of(id)
            .stmts
            .iter()
            .map(|stmt| self.step(*stmt, enclosing))
            .collect::<Option<_>>();
        enclosing.pop();
        Some(Block {
            steps: steps?,
            declares,
        })
    }

    fn step(&self, id: StmtId, enclosing: &mut Vec<Box<[u32]>>) -> Option<Step> {
        let statement = self.stmt(id);
        match &statement.form {
            Form::Plain | Form::If { .. } => {}
            Form::Action(action) => return self.action(id, action),
            _ => return None,
        }
        let mut tree = expr::statement(&self.tables.stream, statement.ops, false)?;
        let step = match (&statement.form, tree.last) {
            (Form::Plain, Op::Break) => {
                let mut left: Vec<u32> = Vec::new();
                for declares in enclosing.iter().rev() {
                    left.extend(declares.iter().rev());
                }
                StepKind::Break(left.into())
            }
            (
                Form::If {
                    then_block,
                    else_block,
                },
                Op::Branch { cond, then_, else_ },
            ) if then_.0 == then_block.0 && else_.0 == else_block.0 => StepKind::If {
                cond: self.test(id, tree.take(cond)?)?,
                then: self.block(*then_block, enclosing)?,
                otherwise: self.block(*else_block, enclosing)?,
            },
            (Form::Plain, Op::WriteLocal { local, src }) if src.0 == 0 => StepKind::Assign {
                slot: self.local(local)?,
                value: self.node(id, tree.take(src)?)?,
            },
            (Form::Plain, Op::Bind { local, src }) if src.0 == 0 => StepKind::Bind {
                slot: self.local(local)?,
                value: self.node(id, tree.take(src)?)?,
            },
            (Form::Plain, Op::SetIndex { target, src }) => {
                self.store(id, &mut tree, target, src, None)?
            }
            (Form::Plain, Op::End) => StepKind::Discard(self.node(id, tree.take(Reg(0))?)?),
            _ => return None,
        };
        tree.spent().then(|| step.into())
    }

    /// A statement whose root action is a call: its value is the first
    /// register of its continuation.
    fn action(&self, id: StmtId, action: &super::Action) -> Option<Step> {
        let statement = self.stmt(id);
        let ActionKind::Call { callee, args } = &action.kind else {
            return None;
        };
        let args = args
            .iter()
            .enumerate()
            .map(|(at, atom)| self.atom(atom, at as u32 + 1))
            .collect::<Option<Box<[Node]>>>()?;
        let site = self.segment.by_stand.get(&(id, None)).copied();
        let kind = match (callee, site) {
            (Callee::Value(Var::Local(local)), Some(site))
                if self.plan.sites.get(site as usize)?.call == SiteCall::Apply =>
            {
                NodeKind::Apply {
                    site,
                    callee: self.slot(local.slot)?,
                    args,
                    // Settled once the whole loop is formed.
                    steady: false,
                }
            }
            (Callee::Library(lib), Some(site))
                if matches!(
                    self.plan.sites.get(site as usize)?.call,
                    SiteCall::Invoke(_)
                ) =>
            {
                NodeKind::Invoke {
                    site,
                    lib: *lib,
                    args: shifted(args)?,
                }
            }
            (Callee::Library(lib), None) => self.called(*lib, shifted(args)?)?,
            _ => return None,
        };
        if let Some(site) = site {
            let stated = self.plan.sites.get(site as usize)?;
            if stated.dst != 0 || !stated.roles.is_empty() {
                return None;
            }
        }
        let act = Node { reg: 0, kind };
        let mut tree = expr::statement(&self.tables.stream, statement.ops, true)?;
        let step = match tree.last {
            Op::Bind { local, src } if src.0 == 0 => {
                tree.take(src)?;
                StepKind::Bind {
                    slot: self.local(local)?,
                    value: act,
                }
            }
            Op::WriteLocal { local, src } if src.0 == 0 => {
                tree.take(src)?;
                StepKind::Assign {
                    slot: self.local(local)?,
                    value: act,
                }
            }
            Op::SetIndex { target, src } => self.store(id, &mut tree, target, src, Some(act))?,
            Op::End => {
                tree.take(Reg(0))?;
                StepKind::Discard(act)
            }
            _ => return None,
        };
        tree.spent().then(|| step.into())
    }

    /// A member write of statement `id`, of the value in `src` to the
    /// target in `target` at the index after it, `act` standing for the
    /// statement's root action.
    fn store(
        &self,
        id: StmtId,
        tree: &mut expr::Statement,
        target: Reg,
        src: Reg,
        act: Option<Node>,
    ) -> Option<StepKind> {
        let mut act = act;
        let mut nodes = [
            tree.take(target)?,
            tree.take(Reg(target.0 + 1))?,
            tree.take(src)?,
        ]
        .map(|expr| (expr.pc, self.acted(id, expr, &mut act)));
        if act.is_some() {
            return None;
        }
        nodes.sort_by_key(|(pc, _)| *pc);
        let [a, b, c] = nodes.map(|(_, node)| node);
        let order = [a?, b?, c?];
        Some(StepKind::Store {
            placed: order[1..].iter().any(Node::stands),
            order,
            target: target.0,
            src: src.0,
        })
    }

    /// `expr` of statement `id`, the root action's value standing where
    /// the tree reads it.
    fn acted(&self, id: StmtId, expr: Expr, act: &mut Option<Node>) -> Option<Node> {
        match expr.kind {
            Kind::Act => act.take(),
            Kind::Pin(inner) => Some(Node {
                reg: expr.reg,
                kind: NodeKind::Pin(Box::new(self.acted(id, *inner, act)?)),
            }),
            _ => self.node(id, expr),
        }
    }

    /// An action's argument read into register `reg`.
    fn atom(&self, atom: &Atom, reg: u32) -> Option<Node> {
        let kind = match atom {
            Atom::Var(Var::Local(local)) => NodeKind::Local(self.slot(local.slot)?),
            Atom::Literal(Constant::Resident(value)) => NodeKind::Const(*value),
            _ => return None,
        };
        self.fits(reg)?;
        Some(Node { reg, kind })
    }

    /// Whether register `reg` is one of the plan's.
    fn fits(&self, reg: u32) -> Option<()> {
        (reg < self.registers).then_some(())
    }

    /// `expr` of statement `id`.
    fn node(&self, id: StmtId, expr: Expr) -> Option<Node> {
        self.fits(expr.reg)?;
        // A nested call the plan invokes at a site may run its body: it is
        // offered there, as the plan's own code offers it.
        let site = match &expr.kind {
            Kind::Operator {
                invoke: Some(invoke),
                ..
            } => self.segment.by_stand.get(&(id, Some(invoke.act))).copied(),
            _ => None,
        };
        let kind = match expr.kind {
            Kind::Local { local, scoped, .. } if !scoped => NodeKind::Local(self.local(local)?),
            Kind::Const(value) => NodeKind::Const(value),
            Kind::Pin(inner) => NodeKind::Pin(Box::new(self.node(id, *inner)?)),
            Kind::Make {
                make: Make::Tuple,
                items,
            } if items.is_empty() => NodeKind::Unit,
            Kind::Operator { lib, args, .. } if site.is_some() => {
                self.invoked(id, site?, expr.reg, lib, args)?
            }
            Kind::Operator {
                lib, invoke, args, ..
            } if invoke.is_none_or(|invoke| invoke.operator) => self.call(id, lib, args)?,
            Kind::Native { lib, args, .. } => self.call(id, lib, args)?,
            _ => return None,
        };
        Some(Node {
            reg: expr.reg,
            kind,
        })
    }

    /// An `if`'s test: its statement grows nothing, so a hole test need
    /// not hold the tuple it compares with.
    fn test(&self, id: StmtId, expr: Expr) -> Option<Node> {
        let node = self.node(id, expr)?;
        Some(match node.kind {
            NodeKind::Call { lib, args, .. }
                if (self.lib)(lib).is_some_and(|lib| lib.definition.name.as_str() == "same")
                    && args.len() == 2
                    && matches!(args[1].kind, NodeKind::Unit) =>
            {
                Node {
                    reg: node.reg,
                    kind: NodeKind::IsUnit { lib, args },
                }
            }
            kind => Node {
                reg: node.reg,
                kind,
            },
        })
    }

    fn call(&self, id: StmtId, lib: LibId, args: Box<[Expr]>) -> Option<NodeKind> {
        self.called(lib, self.nodes(id, args)?)
    }

    /// The general call of `lib` on `args`: a resident list's length read
    /// where it is `list.len`; otherwise fused by the prepared entry it
    /// takes first, where it takes one (a call of every parameter, no
    /// operation, whose entry answers at the plan's depth) and no operand
    /// stands the plan at a site.
    fn called(&self, lib: LibId, args: Box<[Node]>) -> Option<NodeKind> {
        let function = (self.lib)(lib)?;
        let in_place = args.len() <= IN_PLACE && !args.iter().any(Node::stands);
        if function.length_read().is_some() && in_place && args.len() == 1 {
            let list = args.into_vec().into_iter().next()?;
            return Some(NodeKind::Length {
                lib,
                list: Box::new(list),
            });
        }
        let direct = function
            .prepared
            .as_ref()
            .map(|(entry, _)| entry)
            .filter(|entry| {
                in_place
                    && args.len() == function.arity
                    && !function.operation()
                    && entry.depth() == 0
            })
            .cloned();
        Some(NodeKind::Call { lib, args, direct })
    }

    /// A call of `lib` nested in statement `id`, its value to register
    /// `reg`, that the plan invokes at site `site`: offered there when the
    /// plan's site takes its value in that register and holds no other
    /// operand.
    fn invoked(
        &self,
        id: StmtId,
        site: u32,
        reg: u32,
        lib: LibId,
        args: Box<[Expr]>,
    ) -> Option<NodeKind> {
        let stated = self.plan.sites.get(site as usize)?;
        if !matches!(stated.call, SiteCall::Invoke(_))
            || stated.dst != reg
            || !stated.roles.is_empty()
        {
            return None;
        }
        let args = self.nodes(id, args)?;
        Some(NodeKind::Invoke { site, lib, args })
    }

    fn nodes(&self, id: StmtId, args: Box<[Expr]>) -> Option<Box<[Node]>> {
        args.into_vec()
            .into_iter()
            .map(|arg| self.node(id, arg))
            .collect()
    }
}

/// A library call's arguments, from the action's first register.
fn shifted(args: Box<[Node]>) -> Option<Box<[Node]>> {
    Some(
        args.into_vec()
            .into_iter()
            .map(|node| {
                Some(Node {
                    reg: node.reg.checked_sub(1)?,
                    kind: node.kind,
                })
            })
            .collect::<Option<Vec<_>>>()?
            .into(),
    )
}
