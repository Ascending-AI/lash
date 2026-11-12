//! Statements, expressions and actions as ops (`K-FORM-001`,
//! `K-EVAL-008`), with their fixed events (`K-CHG-002`) and sites.
//!
//! Events are queued in the order the rules bill them and attached to the
//! next op: a statement's unit, then each expression node in pre-order,
//! each atom of an action, a loop's test and `metered` units; a
//! construction's members are one atomic event on its `Make` op, after its
//! parts. A site is the chain of `Node::children` indexes from the unit's
//! body, tracked in `Fun::path` as the walk descends.

use std::sync::Arc;

use lash_kernel_doc::{
    ActKind, Action, AssignOrder, Atom, Callee as DocCallee, Expr, FunctionTarget, Literal, Member,
    Place, Protocol, Rhs, Stmt, TryStmt,
};
use lash_kernel_value::names::{CallableId, FunctionId, ImportSlot, Name};
use lash_kernel_value::sites::Site;
use lash_kernel_value::types::Type;

use super::compile::{Compiler, Fun, LoopCtx, Result, Scope, Var, unsupported};
use super::op::Op;
use super::program::{ActionSite, Callee, LoopMark, LoopSite, Point};

impl Compiler<'_> {
    /// Lowers `block` in a scope of its own (`K-FORM-003`).
    pub(crate) fn block(&mut self, f: &mut Fun, block: &[Stmt]) -> Result<()> {
        Self::scoped(f, |f| {
            for (index, stmt) in block.iter().enumerate() {
                f.path.push(index as u32);
                let lowered = self.stmt(f, stmt);
                f.path.pop();
                lowered?;
            }
            Ok(())
        })
    }

    /// Runs `lower` in a new scope: what it declares is visible until it
    /// returns.
    fn scoped<T>(f: &mut Fun, lower: impl FnOnce(&mut Fun) -> Result<T>) -> Result<T> {
        let outer = f.slots.clone();
        let lowered = lower(f);
        f.slots = outer;
        lowered
    }

    /// Lowers `block` as child `index` of the node at `f.path`.
    fn child_block(&mut self, f: &mut Fun, index: u32, block: &[Stmt]) -> Result<()> {
        f.path.push(index);
        let lowered = self.block(f, block);
        f.path.pop();
        lowered
    }

    fn child(&mut self, f: &mut Fun, index: u32, value: &Expr) -> Result<()> {
        f.path.push(index);
        let lowered = self.expr(f, value);
        f.path.pop();
        lowered
    }

    // --- Variables.

    fn resolve(&mut self, f: &Fun, name: &Name) -> Var {
        match (f.slots.get(name), f.captures.get(name)) {
            (Some(slot), _) if f.boxed.contains(name) => Var::Cell(*slot),
            (Some(slot), _) => Var::Local(*slot),
            (None, Some(capture)) => Var::Capture(*capture),
            (None, None) => Var::Global(self.global(name)),
        }
    }

    fn read(&mut self, f: &mut Fun, name: &Name) {
        let op = match self.resolve(f, name) {
            Var::Local(slot) => Op::Local(slot),
            Var::Cell(slot) => Op::CellGet(slot),
            Var::Capture(capture) => Op::CaptureGet(capture),
            Var::Global(global) => Op::Global(global),
        };
        f.emit(op);
    }

    fn write(&mut self, f: &mut Fun, name: &Name) {
        let op = match self.resolve(f, name) {
            Var::Local(slot) => Op::SetLocal(slot),
            Var::Cell(slot) => Op::CellSet(slot),
            Var::Capture(capture) => Op::CaptureSet(capture),
            Var::Global(global) => Op::SetGlobal(global),
        };
        f.emit(op);
    }

    /// A declaration: each execution makes a new variable, so a shared one
    /// gets a fresh cell (closures made by earlier executions keep theirs).
    fn bind(f: &mut Fun, name: &Name) {
        let slot = f.declare(name);
        f.emit(Op::SetLocal(slot));
        if f.boxed.contains(name) {
            f.emit(Op::Box(slot));
        }
    }

    /// An op that leaves its block for good: what follows starts a block.
    fn exit(f: &mut Fun, op: Op) -> usize {
        let at = f.emit(op);
        f.leader();
        at
    }

    // --- Statements.

    fn stmt(&mut self, f: &mut Fun, stmt: &Stmt) -> Result<()> {
        let site = f.site();
        let outer = std::mem::replace(&mut f.statement, site);
        let lowered = self.statement(f, stmt);
        f.statement = outer;
        lowered
    }

    fn statement(&mut self, f: &mut Fun, stmt: &Stmt) -> Result<()> {
        if f.observed() {
            f.start = Some(Point::Start(f.statement.clone()));
        }
        f.mark = true;
        f.units(1);
        match stmt {
            Stmt::Let { name, value } => {
                self.rhs(f, 0, value)?;
                Self::bind(f, name);
            }
            Stmt::Assign {
                place: Place::Variable(name),
                value,
                ..
            } => {
                self.rhs(f, 0, value)?;
                self.write(f, name);
            }
            Stmt::Assign {
                place: Place::Member(member),
                value,
                order,
            } => self.assign(f, member, value, *order)?,
            Stmt::Remove { member } => match member {
                Member::Index { target, index } => {
                    self.child(f, 0, target)?;
                    self.child(f, 1, index)?;
                    f.emit(Op::RemoveIndex);
                }
                Member::Field { target, field } => {
                    self.child(f, 0, target)?;
                    let field = self.text(field);
                    f.emit(Op::RemoveField(field));
                }
            },
            Stmt::Do { value } => {
                self.child(f, 0, value)?;
                f.emit(Op::Pop);
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.child(f, 0, condition)?;
                let test = f.emit(Op::JumpIfFalse(0));
                // An empty branch is entered and left as the test is taken.
                let empty = |index, block: &[Stmt]| block.is_empty().then(|| f.site().child(index));
                let point = Point::Branch {
                    then: empty(1, then_block),
                    otherwise: empty(2, else_block),
                };
                f.point(test as u32, point);
                f.leader();
                self.child_block(f, 1, then_block)?;
                if else_block.is_empty() {
                    let end = f.label();
                    f.patch(test, end);
                } else {
                    let skip = Self::exit(f, Op::Jump(0));
                    let otherwise = f.label();
                    f.patch(test, otherwise);
                    self.child_block(f, 2, else_block)?;
                    let end = f.label();
                    f.patch(skip, end);
                }
            }
            Stmt::While {
                metered,
                condition,
                body,
                else_block,
            } => {
                let started = Self::enter_loop(f);
                let head = f.label();
                f.point(head, Point::Test(f.statement.clone()));
                self.loop_test(f, *metered);
                if *metered != 0 {
                    // The metered event comes before the condition's.
                    f.emit(Op::Nop);
                }
                self.child(f, 0, condition)?;
                let test = f.emit(Op::WhileFalse(0));
                f.leader();
                f.quiet(Op::Pass(started));
                f.loops.push(LoopCtx {
                    head,
                    breaks: Vec::new(),
                });
                let lowered = self.child_block(f, 1, body);
                let ctx = f.loops.pop();
                lowered?;
                Self::exit(f, Op::Jump(head));
                f.enclosing.pop();
                let exhausted = f.label();
                f.patch(test, exhausted);
                if !else_block.is_empty() {
                    self.child_block(f, 2, else_block)?;
                }
                let end = f.label();
                for at in ctx.into_iter().flat_map(|ctx| ctx.breaks) {
                    f.patch(at, end);
                }
            }
            Stmt::For {
                metered,
                binding,
                iterable,
                via,
                body,
                else_block,
            } => {
                self.child(f, 0, iterable)?;
                // The binding is declared in the body's scope alone.
                Self::scoped(f, |f| match via {
                    Some(protocol) => self.protocol_for(f, *metered, binding, protocol, body),
                    None => self.plain_for(f, *metered, binding, body),
                })?;
                if !else_block.is_empty() {
                    self.child_block(f, 2, else_block)?;
                }
                let end = f.label();
                if let Some(ctx) = f.loops.pop() {
                    for at in ctx.breaks {
                        f.patch(at, end);
                    }
                }
            }
            Stmt::Break => {
                let depth = f.loops.len();
                self.depart(f, Some(depth))?;
                let at = Self::exit(f, Op::Jump(0));
                match f.loops.last_mut() {
                    Some(ctx) => ctx.breaks.push(at),
                    None => return unsupported("`break` outside a loop"),
                }
            }
            Stmt::Continue => {
                let depth = f.loops.len();
                self.depart(f, Some(depth))?;
                let Some(head) = f.loops.last().map(|ctx| ctx.head) else {
                    return unsupported("`continue` outside a loop");
                };
                Self::exit(f, Op::Jump(head));
            }
            Stmt::Return { value } => {
                self.child(f, 0, value)?;
                if Self::has_finally(f, None) {
                    // The value is held while the finally blocks run.
                    let held = f.temp();
                    f.emit(Op::SetLocal(held));
                    self.depart(f, None)?;
                    f.emit(Op::Local(held));
                } else {
                    self.depart(f, None)?;
                }
                Self::exit(f, Op::Return);
            }
            Stmt::ReturnVia { value, policy } => {
                self.child(f, 0, value)?;
                // Cleanup unwinds first; the policy runs outside the
                // function's handlers and its result returns directly.
                let held = f.temp();
                f.emit(Op::SetLocal(held));
                self.depart(f, None)?;
                f.emit(Op::Local(held));
                let policy = Self::local_target(policy, "a return policy")?;
                let site = f.site();
                let callee = self.callee(f, policy)?;
                self.call(f, callee, 1, site);
                Self::exit(f, Op::Return);
            }
            Stmt::Try(scope) => self.try_stmt(f, scope)?,
            Stmt::Throw { value } => {
                self.child(f, 0, value)?;
                Self::exit(f, Op::Raise);
            }
            Stmt::Print { value } => {
                self.child(f, 0, value)?;
                f.emit(Op::Print);
            }
            Stmt::Finish { value } => {
                self.child(f, 0, value)?;
                Self::exit(f, Op::Finish);
            }
            Stmt::Fail { value } => {
                self.child(f, 0, value)?;
                Self::exit(f, Op::Fail);
            }
        }
        Ok(())
    }

    /// Enters a loop at the statement being lowered: its pass counter is
    /// zeroed, and it encloses what is lowered until its else block.
    fn enter_loop(f: &mut Fun) -> u16 {
        let started = f.temp();
        f.quiet(Op::LoopStart(started));
        f.enclosing.push(LoopMark {
            site: f.statement.clone(),
            started,
        });
        started
    }

    /// A loop's continuation test: its unit, then its `metered` event,
    /// which charges under a cover too (`K-CHG-002`, `K-CHG-007`). The
    /// next op bills both.
    fn loop_test(&mut self, f: &mut Fun, metered: u64) {
        f.units(1);
        f.metered(metered);
    }

    fn assign(
        &mut self,
        f: &mut Fun,
        member: &Member,
        value: &Rhs,
        order: AssignOrder,
    ) -> Result<()> {
        // The place's children come first, then the value.
        let value_index = match member {
            Member::Field { .. } => 1,
            Member::Index { .. } => 2,
        };
        let place_first = matches!(order, AssignOrder::PlaceFirst) && matches!(value, Rhs::Expr(_));
        if !place_first {
            // An action's result, like an expression's, is ready before
            // the place is read (`K-EVAL-003`).
            self.rhs(f, value_index, value)?;
        }
        match member {
            Member::Field { target, field } => {
                self.child(f, 0, target)?;
                let field = self.text(field);
                if place_first {
                    self.rhs(f, value_index, value)?;
                    f.emit(Op::SetFieldPlaceFirst(field));
                } else {
                    f.emit(Op::SetField(field));
                }
            }
            Member::Index { target, index } => {
                self.child(f, 0, target)?;
                self.child(f, 1, index)?;
                if place_first {
                    self.rhs(f, value_index, value)?;
                    f.emit(Op::SetIndexPlaceFirst);
                } else {
                    f.emit(Op::SetIndex);
                }
            }
        }
        Ok(())
    }

    fn plain_for(
        &mut self,
        f: &mut Fun,
        metered: u64,
        binding: &Name,
        body: &[Stmt],
    ) -> Result<()> {
        let slot = f.temp();
        f.temp(); // the cursor
        let index = f.loop_sites.len() as u32;
        let binding_slot = f.declare(binding);
        f.loop_sites.push(LoopSite {
            slot,
            binding: binding_slot,
            exit: 0,
            driver: None,
            join: None,
            site: f.site(),
        });
        let started = Self::enter_loop(f);
        f.emit(Op::ForOpen(index));
        let head = f.label();
        self.loop_test(f, metered);
        f.emit(Op::ForNext(index));
        f.leader();
        if f.boxed.contains(binding) {
            f.emit(Op::Box(binding_slot));
        }
        f.quiet(Op::Pass(started));
        f.loops.push(LoopCtx {
            head,
            breaks: Vec::new(),
        });
        self.child_block(f, 1, body)?;
        Self::exit(f, Op::Jump(head));
        f.enclosing.pop();
        let exit = f.label();
        f.loop_sites[index as usize].exit = exit;
        Ok(())
    }

    /// `for x in e via P` (`K-ITER-005`), the iterable on the stack. Each
    /// hook is an ordinary library call (R2 ruling A); the header bills
    /// the loop test, then steps, checks a guarded map or set, and binds
    /// through `item`. The loop's context stays pushed for the caller.
    fn protocol_for(
        &mut self,
        f: &mut Fun,
        metered: u64,
        binding: &Name,
        protocol: &Protocol,
        body: &[Stmt],
    ) -> Result<()> {
        let open = Self::local_target(&protocol.open, "a loop protocol's `open`")?;
        let hook = |target: &Option<FunctionTarget>| -> Result<Option<FunctionId>> {
            target
                .as_ref()
                .map(|target| Self::local_target(target, "a loop protocol hook"))
                .transpose()
        };
        let (next, item, check) = (
            hook(&protocol.next)?,
            hook(&protocol.item)?,
            hook(&protocol.check)?,
        );
        let site = f.site();
        let started = Self::enter_loop(f);
        let callee = self.callee(f, open)?;
        self.call(f, callee, 1, site.clone());
        // walked, cursor, size, remaining, raw item, more.
        let slot = f.temp();
        for _ in 0..5 {
            f.temp();
        }
        let binding_slot = f.declare(binding);
        let index = f.loop_sites.len() as u32;
        f.loop_sites.push(LoopSite {
            slot,
            binding: binding_slot,
            exit: 0,
            driver: None,
            join: None,
            site: site.clone(),
        });
        f.emit(Op::ProtoOpen(index));
        let head = f.label();
        self.loop_test(f, metered);
        f.emit(Op::ProtoStep(index));
        f.leader();
        if let Some(check) = check {
            f.emit(Op::ProtoGuard(index));
            f.leader();
            let callee = self.callee(f, check)?;
            self.call(f, callee, 4, site.clone());
            f.emit(Op::ProtoChecked(index));
        }
        let join = f.label();
        f.emit(Op::ProtoMore(index));
        f.leader();
        match item {
            Some(item) => {
                f.emit(Op::Local(slot));
                f.emit(Op::Local(slot + 4));
                let callee = self.callee(f, item)?;
                self.call(f, callee, 2, site.clone());
            }
            None => {
                f.emit(Op::Local(slot + 4));
            }
        }
        f.emit(Op::SetLocal(binding_slot));
        if f.boxed.contains(binding) {
            f.emit(Op::Box(binding_slot));
        }
        f.quiet(Op::Pass(started));
        f.leader();
        f.loops.push(LoopCtx {
            head,
            breaks: Vec::new(),
        });
        self.child_block(f, 1, body)?;
        Self::exit(f, Op::Jump(head));
        // The driver's step: `next` on what the loop walks. Without a
        // `next`, what `open` gives must be walkable (`K-ITER-005`).
        let driver = match next {
            Some(next) => {
                let driver = f.label();
                f.emit(Op::Local(slot));
                let callee = self.callee(f, next)?;
                self.call(f, callee, 1, site);
                f.emit(Op::ProtoPull(index));
                Self::exit(f, Op::Jump(join));
                Some(driver)
            }
            None => None,
        };
        f.enclosing.pop();
        let exit = f.label();
        let loop_site = &mut f.loop_sites[index as usize];
        loop_site.exit = exit;
        loop_site.driver = driver;
        loop_site.join = Some(join);
        Ok(())
    }

    fn try_stmt(&mut self, f: &mut Fun, scope: &TryStmt) -> Result<()> {
        let depth = f.loops.len();
        let finally_index = 1 + u32::from(scope.catch.is_some());
        let finally_path = {
            let mut path = f.path.clone();
            path.push(finally_index);
            path
        };
        let outer = match &scope.finally {
            Some(finally) => {
                let block: Arc<[Stmt]> = Arc::from(finally.as_slice());
                f.scopes
                    .push((depth, Scope::Finally(block.clone(), finally_path.clone())));
                let enter = f.emit(Op::TryEnter(0));
                f.scopes.push((depth, Scope::Handler));
                Some((enter, block))
            }
            None => None,
        };
        match &scope.catch {
            Some(catch) => {
                let enter = f.emit(Op::TryEnter(0));
                f.scopes.push((depth, Scope::Handler));
                let lowered = self.child_block(f, 0, &scope.body);
                f.scopes.pop();
                lowered?;
                f.emit(Op::TryExit);
                let skip = Self::exit(f, Op::Jump(0));
                // The handler starts a block, the raised value on the stack.
                let handler = f.label();
                f.patch(enter, handler);
                let taking = f.emit(Op::CatchEnter(0));
                f.scopes.push((depth, Scope::Catch(taking as u32)));
                Self::scoped(f, |f| {
                    Self::bind(f, &catch.binding);
                    self.child_block(f, 1, &catch.body)
                })?;
                f.scopes.pop();
                f.emit(Op::CatchExit(taking as u32));
                f.patch(taking, f.here());
                let end = f.label();
                f.patch(skip, end);
            }
            None => self.child_block(f, 0, &scope.body)?,
        }
        if let Some((enter, block)) = outer {
            f.scopes.pop();
            f.scopes.pop();
            // Normal completion leaves the handler and runs the block.
            f.emit(Op::TryExit);
            self.finally_block(f, &block, &finally_path)?;
            let skip = Self::exit(f, Op::Jump(0));
            // A raise: hold the value, run the block, raise it again.
            let handler = f.label();
            f.patch(enter, handler);
            let held = f.temp();
            f.emit(Op::SetLocal(held));
            self.finally_block(f, &block, &finally_path)?;
            f.emit(Op::Local(held));
            Self::exit(f, Op::Raise);
            let end = f.label();
            f.patch(skip, end);
        }
        Ok(())
    }

    fn finally_block(&mut self, f: &mut Fun, block: &[Stmt], path: &[u32]) -> Result<()> {
        let saved = std::mem::replace(&mut f.path, path.to_vec());
        let lowered = self.block(f, block);
        f.path = saved;
        lowered
    }

    /// Leaves the scopes a departure passes, innermost first: handlers are
    /// exited and finally blocks run inline (`K-FORM-017`). `to` is the
    /// loop depth a `break` or `continue` stays in; `None` leaves all.
    fn depart(&mut self, f: &mut Fun, to: Option<usize>) -> Result<()> {
        let saved = f.scopes.clone();
        let mut at = saved.len();
        while at > 0 {
            at -= 1;
            let (depth, scope) = saved[at].clone();
            if to.is_some_and(|stay| depth < stay) {
                break;
            }
            match scope {
                Scope::Handler => {
                    f.emit(Op::TryExit);
                }
                Scope::Catch(taking) => {
                    f.emit(Op::CatchExit(taking));
                }
                Scope::Finally(block, path) => {
                    f.scopes.truncate(at);
                    let lowered = self.finally_block(f, &block, &path);
                    if lowered.is_err() {
                        f.scopes = saved;
                        return lowered;
                    }
                }
            }
        }
        f.scopes = saved;
        Ok(())
    }

    fn has_finally(f: &Fun, to: Option<usize>) -> bool {
        f.scopes.iter().any(|(depth, scope)| {
            matches!(scope, Scope::Finally(..)) && to.is_none_or(|stay| *depth >= stay)
        })
    }

    fn local_target(target: &FunctionTarget, what: &str) -> Result<FunctionId> {
        match target {
            FunctionTarget::Local(id) => Ok(*id),
            _ => unsupported(format!("{what} that names an import")),
        }
    }

    // --- Right-hand sides and actions.

    /// Lowers child `index`: an expression, or an action whose atoms are
    /// its arguments.
    fn rhs(&mut self, f: &mut Fun, index: u32, value: &Rhs) -> Result<()> {
        let action = match value {
            Rhs::Expr(value) => return self.child(f, index, value),
            Rhs::Action(action) => action,
        };
        f.path.push(index);
        let lowered = self.action(f, action);
        f.path.pop();
        lowered
    }

    fn action(&mut self, f: &mut Fun, action: &Action) -> Result<()> {
        let site = f.site();
        match action {
            Action::Call { callee, args } => {
                self.callee_value(f, callee);
                self.atoms(f, args, false)?;
                self.call_callee(f, callee, args.len(), site)?;
            }
            Action::Perform {
                slot,
                args,
                result,
                then_yield,
            } => {
                self.atoms(f, args, true)?;
                self.perform(f, slot, args.len(), result, *then_yield, site);
            }
            Action::Join { task } => {
                self.atoms(f, std::slice::from_ref(task), false)?;
                self.task_action(f, ActionSite::Join { site });
            }
            Action::JoinMany { mode, tasks } => {
                self.atoms(f, std::slice::from_ref(tasks), false)?;
                self.task_action(f, ActionSite::JoinMany { mode: *mode, site });
            }
            Action::Yield => self.task_action(f, ActionSite::Yield { site }),
            Action::Cancel { task } => {
                self.atoms(f, std::slice::from_ref(task), false)?;
                self.task_action(f, ActionSite::Cancel { site });
            }
            Action::CancelEffect { task } => {
                self.atoms(f, std::slice::from_ref(task), false)?;
                self.task_action(f, ActionSite::CancelEffect { site });
            }
            Action::Spawn { callee, args } => {
                self.callee_value(f, callee);
                self.atoms(f, args, true)?;
                self.spawn(f, callee, args.len(), site)?;
            }
            Action::Await { policy, value } => {
                self.atoms(f, std::slice::from_ref(value), false)?;
                let policy = Self::local_target(policy, "an await policy")?;
                let callee = self.callee(f, policy)?;
                self.entered_call(f, callee, 1, site);
            }
        }
        Ok(())
    }

    /// An action's atoms, one event each; a perform's or spawn's charge
    /// under a cover too (`K-CHG-007`).
    fn atoms(&mut self, f: &mut Fun, args: &[Atom], always: bool) -> Result<()> {
        for arg in args {
            if always {
                f.always(1);
            } else {
                f.units(1);
            }
            match arg {
                Atom::Variable(name) => self.read(f, name),
                Atom::Literal(literal) => self.literal(f, literal)?,
            }
        }
        Ok(())
    }

    fn literal(&mut self, f: &mut Fun, literal: &Literal) -> Result<()> {
        if let Literal::Function(id) = literal {
            self.trampoline(*id)?;
        }
        let at = self.constant(literal);
        f.emit(Op::Const(at));
        Ok(())
    }

    /// A value callee is read before the arguments (`K-EVAL-004`).
    fn callee_value(&mut self, f: &mut Fun, callee: &DocCallee) {
        if let DocCallee::Value(name) = callee {
            self.read(f, name);
        }
    }

    fn call_callee(
        &mut self,
        f: &mut Fun,
        callee: &DocCallee,
        argc: usize,
        site: Site,
    ) -> Result<()> {
        let (target, checked) = match callee {
            DocCallee::Value(_) => (Callee::Value, None),
            DocCallee::Local(id) => {
                let target = self.callee(f, *id)?;
                let checked = self.checked(f, *id, &target);
                (target, checked)
            }
            DocCallee::Import(slot) => self.import_call(f, *slot)?,
        };
        f.point(f.here(), Point::Action(site.clone()));
        self.entered_call(f, target, argc as u16, site);
        if let Some(call) = f.calls.last_mut() {
            call.checked = checked;
        }
        Ok(())
    }

    fn perform(
        &mut self,
        f: &mut Fun,
        slot: &ImportSlot,
        argc: usize,
        result: &Type,
        then_yield: bool,
        site: Site,
    ) {
        self.task_action(
            f,
            ActionSite::Perform {
                owner: f.owner,
                slot: *slot,
                argc: argc as u16,
                result: result.clone(),
                then_yield,
                site,
            },
        );
    }

    fn spawn(&mut self, f: &mut Fun, callee: &DocCallee, argc: usize, site: Site) -> Result<()> {
        let (target, resolved) = match callee {
            DocCallee::Value(_) => (None, None),
            DocCallee::Local(id) => (
                Some(CallableId {
                    link: self.link,
                    function: *id,
                }),
                None,
            ),
            DocCallee::Import(slot) => {
                let callable = self.imported(*slot)?;
                (Some(callable), Some(self.import_call(f, *slot)?))
            }
        };
        let unit = self.stub(f, target, resolved, argc as u16)?;
        let values = argc as u16 + u16::from(target.is_none());
        self.task_action(
            f,
            ActionSite::Spawn {
                unit,
                argc: values,
                site,
            },
        );
        Ok(())
    }

    /// An action other than a call: it ends its block, so a task resumes
    /// at a block head (`K-EVAL-008`).
    fn task_action(&mut self, f: &mut Fun, action: ActionSite) {
        let at = f.actions.len() as u32;
        let mark = self.mark(f, action.site());
        f.actions.push(action);
        f.action_marks.push(mark);
        let pc = f.emit(Op::Action(at));
        f.point(
            pc as u32,
            Point::Action(f.actions[at as usize].site().clone()),
        );
        f.leader();
    }

    // --- Expressions.

    fn expr(&mut self, f: &mut Fun, value: &Expr) -> Result<()> {
        f.units(1);
        match value {
            Expr::Literal(literal) => self.literal(f, literal)?,
            Expr::Variable(name) => self.read(f, name),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                for (index, item) in items.iter().enumerate() {
                    self.child(f, index as u32, item)?;
                }
                let n = items.len() as u32;
                f.members(u64::from(n));
                f.emit(match value {
                    Expr::Tuple(_) => Op::MakeTuple(n),
                    Expr::List(_) => Op::MakeList(n),
                    _ => Op::MakeSet(n),
                });
            }
            Expr::Map(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    self.child(f, 2 * index as u32, &entry.key)?;
                    self.child(f, 2 * index as u32 + 1, &entry.value)?;
                }
                f.members(entries.len() as u64);
                f.emit(Op::MakeMap(entries.len() as u32));
            }
            Expr::Record(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    self.child(f, index as u32, &entry.value)?;
                }
                let shape: Arc<[u32]> = entries
                    .iter()
                    .map(|entry| self.text(&entry.field))
                    .collect();
                let at = f.records.len() as u32;
                f.records.push(shape);
                f.members(entries.len() as u64);
                f.emit(Op::MakeRecord(at));
            }
            Expr::Member(member) => match &**member {
                Member::Field { target, field } => {
                    self.child(f, 0, target)?;
                    let field = self.text(field);
                    f.emit(Op::Field(field));
                }
                Member::Index { target, index } => {
                    self.child(f, 0, target)?;
                    self.child(f, 1, index)?;
                    f.emit(Op::Index);
                }
            },
            Expr::Closure(closure) => {
                let at = self.closure(f, closure.function)?;
                f.emit(Op::MakeClosure(at));
            }
            Expr::Call { function, args } => {
                if let Some(at) = self.brand_built(f, *function)? {
                    // Answered where it is written: the owner's identity.
                    f.emit(Op::Const(at));
                    return Ok(());
                }
                for (index, arg) in args.iter().enumerate() {
                    self.child(f, index as u32, arg)?;
                }
                let site = f.site();
                let callee = self.callee(f, *function)?;
                self.call(f, callee, args.len() as u16, site);
            }
            Expr::If { cond, then, r#else } => {
                self.child(f, 0, cond)?;
                let test = f.emit(Op::JumpIfFalse(0));
                f.leader();
                self.child(f, 1, then)?;
                let skip = Self::exit(f, Op::Jump(0));
                let otherwise = f.label();
                f.patch(test, otherwise);
                self.child(f, 2, r#else)?;
                let end = f.label();
                f.patch(skip, end);
            }
            Expr::Let { name, value, body } => {
                self.child(f, 0, value)?;
                Self::scoped(f, |f| {
                    Self::bind(f, name);
                    self.child(f, 1, body)
                })?;
            }
            Expr::Act(act) => {
                let site = f.site();
                if let ActKind::Call(callee) | ActKind::Spawn(callee) = &act.kind {
                    self.callee_value(f, callee);
                }
                for (index, arg) in act.args.iter().enumerate() {
                    self.child(f, index as u32, arg)?;
                }
                let argc = act.args.len();
                match &act.kind {
                    ActKind::Call(callee) => self.call_callee(f, callee, argc, site)?,
                    ActKind::Perform {
                        slot,
                        result,
                        then_yield,
                    } => self.perform(f, slot, argc, result, *then_yield, site),
                    ActKind::Spawn(callee) => self.spawn(f, callee, argc, site)?,
                    ActKind::Join => self.task_action(f, ActionSite::Join { site }),
                    ActKind::JoinMany(mode) => {
                        self.task_action(f, ActionSite::JoinMany { mode: *mode, site })
                    }
                    ActKind::Yield => self.task_action(f, ActionSite::Yield { site }),
                    ActKind::Cancel => self.task_action(f, ActionSite::Cancel { site }),
                    ActKind::CancelEffect => self.task_action(f, ActionSite::CancelEffect { site }),
                    ActKind::Await(policy) => {
                        let policy = Self::local_target(policy, "an await policy")?;
                        let callee = self.callee(f, policy)?;
                        f.point(f.here(), Point::Action(site.clone()));
                        self.entered_call(f, callee, 1, site);
                    }
                }
            }
        }
        Ok(())
    }
}
