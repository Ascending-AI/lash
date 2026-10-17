//! Lexical scopes, closure captures and frame-slot placement.

use super::*;
use lash_kernel_exec::LocalRef;

impl Compiler<'_> {
    pub(super) fn site(&self) -> Site {
        Site::new(self.unit.clone(), self.path.clone())
    }

    /// Compiles one unit's body into the code slot reserved for it.
    pub(super) fn unit_code(
        &mut self,
        code: CodeId,
        unit: Unit,
        params: &[Name],
        body: &doc::Block,
        charged: bool,
    ) {
        let main = unit == Unit::Main;
        let free = matches!(unit, Unit::Function(_));
        self.unit = unit;
        self.path.clear();
        let compiled = self.code(params, body, main, charged, free);
        self.codes[(code.0 - self.base.codes) as usize] = Some(compiled);
    }

    /// Compiles a function body in a scope of its own. The path addresses
    /// the body block.
    pub(super) fn code(
        &mut self,
        params: &[Name],
        body: &doc::Block,
        main: bool,
        charged: bool,
        free: bool,
    ) -> Code {
        let site = self.site();
        // A body's statements have roles of their own: the operands of an
        // enclosing expression are no role of theirs.
        let enclosing = std::mem::take(&mut self.live);
        let enclosing_depth = self.stmt_depth;
        self.scopes.push(Scopes {
            main,
            body: site.clone(),
            blocks: Vec::new(),
            slots: Vec::new(),
            captures: Vec::new(),
            free: free.then(Vec::new),
        });
        let (body, params) = self.block(body, params);
        let scopes = self.scopes.pop().unwrap_or(Scopes {
            main,
            body: site.clone(),
            blocks: Vec::new(),
            slots: Vec::new(),
            captures: Vec::new(),
            free: None,
        });
        self.live = enclosing;
        self.stmt_depth = enclosing_depth;
        let mut positions: Vec<u32> = (0..scopes.slots.len() as u32).collect();
        self.shuffler.shuffle(self.layout, &mut positions);
        let placer = Placer {
            positions: &positions,
            slots: &scopes.slots,
        };
        let params: Vec<Local> = params
            .into_iter()
            .map(|slot| placer.placed(Local::unplaced(slot)))
            .collect();
        let free = scopes
            .free
            .iter()
            .flatten()
            .map(|(name, slot)| (name.clone(), placer.placed(Local::unplaced(*slot))))
            .collect();
        let mut captures = scopes.captures;
        for capture in &mut captures {
            capture.inner_at = positions[capture.inner as usize];
        }
        // Lowered, every variable already sits at its slot and is bound as
        // unshared: a code whose layout keeps the slots in place and that
        // shares none is placed as it is.
        let moved = positions
            .iter()
            .enumerate()
            .any(|(slot, at)| slot as u32 != *at);
        if moved || scopes.slots.iter().any(|slot| slot.shared) {
            self.place(body, &placer);
        }
        let mut slots_by_at = vec![0; positions.len()];
        for (slot, at) in positions.iter().enumerate() {
            slots_by_at[*at as usize] = slot as u32;
        }
        Code {
            versions: BlockVersions::new(params.len(), self.layout),
            site,
            params,
            body,
            slots: scopes.slots,
            slots_by_at,
            captures,
            free,
            charged,
        }
    }

    /// Writes where each variable of a code sits into the ops and the
    /// shapes of its statements, which its blocks hold, and into the
    /// captures of the closures it makes.
    fn place(&mut self, body: BlockId, placer: &Placer<'_>) {
        let mut blocks = vec![body];
        let mut closures = Vec::new();
        while let Some(block) = blocks.pop() {
            let block = &mut self.blocks[(block.0 - self.base.blocks) as usize];
            for declared in &mut block.declares {
                *declared = placer.positions[*declared as usize];
            }
            for stmt in &block.stmts {
                let stmt = &mut self.stmts[(stmt.0 - self.base.stmts) as usize];
                placer.form(&mut stmt.form, &mut blocks);
                let mut ranges = [Some(stmt.ops), None];
                if let Form::While { test, .. } = stmt.form {
                    ranges[1] = Some(test);
                }
                for range in ranges.into_iter().flatten() {
                    let ops = &mut self.stream.ops[range.start as usize..range.end() as usize];
                    placer.ops(ops, &mut closures);
                }
            }
        }
        for closure in closures {
            if let Some(Some(code)) = self.codes.get_mut((closure.0 - self.base.codes) as usize) {
                for capture in &mut code.captures {
                    capture.outer_at = placer.positions[capture.outer as usize];
                }
            }
        }
    }

    /// The variable of the document function being compiled that holds its
    /// free name `name`, taken on its first read; none for other code.
    fn free_slot(&mut self, name: &Name) -> Option<Slot> {
        let scopes = self.scopes.first_mut()?;
        let free = scopes.free.as_mut()?;
        if let Some((_, slot)) = free.iter().find(|(taken, _)| taken == name) {
            return Some(*slot);
        }
        let slot = scopes.slots.len() as Slot;
        scopes.slots.push(SlotInfo {
            key: lash_kernel_state::kernel_internals::metadata::metadata_key(&(
                scopes.body.clone(),
                name.clone(),
            )),
            name: name.clone(),
            declared: scopes.body.clone(),
            shared: false,
        });
        free.push((name.clone(), slot));
        Some(slot)
    }

    /// Declares a variable at the node being compiled.
    pub(super) fn declare(&mut self, name: &Name) -> Slot {
        let declared = self.site();
        let Some(scopes) = self.scopes.last_mut() else {
            return 0;
        };
        let slot = scopes.slots.len() as Slot;
        scopes.slots.push(SlotInfo {
            key: lash_kernel_state::kernel_internals::metadata::metadata_key(&(
                declared.clone(),
                name.clone(),
            )),
            name: name.clone(),
            declared,
            shared: false,
        });
        if let Some(block) = scopes.blocks.last_mut() {
            block.push((name.clone(), Declared::Slot(slot)));
        }
        slot
    }

    /// Compiles a block, declaring `bound` in its scope first: a function's
    /// parameters, a loop's binding, a catch's binding.
    pub(super) fn block(&mut self, block: &doc::Block, bound: &[Name]) -> (BlockId, Vec<Slot>) {
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
                Declared::Slot(slot) | Declared::ScopedSlot(slot) => Some(slot),
                Declared::Session => None,
            })
            .collect();
        // Statement and block ids are taken in the order the layout visits
        // the units, so they differ between layouts.
        let id = BlockId(self.base.blocks + self.blocks.len() as u32);
        self.blocks.push(Block {
            site: self.site(),
            stmts: Vec::new(),
            declares,
        });
        let ids = stmts
            .into_iter()
            .map(|stmt| {
                let id = StmtId(self.base.stmts + self.stmts.len() as u32);
                self.stmts.push(stmt);
                id
            })
            .collect();
        self.blocks[(id.0 - self.base.blocks) as usize].stmts = ids;
        (id, bound)
    }

    pub(super) fn child<T>(&mut self, index: u32, compile: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(index);
        let compiled = compile(self);
        self.path.pop();
        compiled
    }

    pub(super) fn resolve(&mut self, name: &Name) -> Var {
        match self.scopes.len() {
            0 => Var::Unbound(name.clone()),
            depth => self.resolve_in(depth - 1, name),
        }
    }

    /// Resolves `name` as code `level` sees it. A variable of an enclosing
    /// code becomes a capture of every closure between the two.
    pub(super) fn resolve_in(&mut self, level: usize, name: &Name) -> Var {
        let scopes = &self.scopes[level];
        let declared = scopes
            .blocks
            .iter()
            .rev()
            .flat_map(|block| block.iter().rev())
            .find(|(declared, kind)| declared == name && !matches!(kind, Declared::ScopedSlot(_)))
            .map(|(_, declared)| *declared);
        match declared {
            Some(Declared::Slot(slot)) => return Var::Local(Local::unplaced(slot)),
            Some(Declared::Session) => return Var::Session(super::BindingName::new(name.clone())),
            None | Some(Declared::ScopedSlot(_)) => {}
        }
        if level == 0 {
            if scopes.main {
                return Var::Session(super::BindingName::new(name.clone()));
            }
            return match self.free_slot(name) {
                Some(slot) => Var::Local(Local::unplaced(slot)),
                None => Var::Unbound(name.clone()),
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
            return Var::Local(Local::unplaced(inner));
        }
        match self.resolve_in(level - 1, name) {
            Var::Local(Local { slot: outer, .. }) => {
                self.scopes[level - 1].slots[outer as usize].shared = true;
                let scopes = &mut self.scopes[level];
                let inner = scopes.slots.len() as Slot;
                scopes.slots.push(SlotInfo {
                    key: lash_kernel_state::kernel_internals::metadata::metadata_key(&(
                        scopes.body.clone(),
                        name.clone(),
                    )),
                    name: name.clone(),
                    declared: scopes.body.clone(),
                    shared: true,
                });
                scopes.captures.push(Capture {
                    outer,
                    inner,
                    outer_at: outer,
                    inner_at: inner,
                });
                Var::Local(Local::unplaced(inner))
            }
            other => other,
        }
    }
}

/// Where the variables of one compiled code sit in its frames.
struct Placer<'p> {
    positions: &'p [u32],
    slots: &'p [SlotInfo],
}

impl Placer<'_> {
    fn placed(&self, local: Local) -> Local {
        Local {
            slot: local.slot,
            at: self.positions[local.slot as usize],
            shared: self.slots[local.slot as usize].shared,
        }
    }

    fn local(&self, local: &mut Local) {
        *local = self.placed(*local);
    }

    fn var(&self, var: &mut Var) {
        if let Var::Local(local) = var {
            self.local(local);
        }
    }

    /// Places a statement's ops' variables, a binding of a variable a
    /// closure shares as its cell, and collects the closures they make.
    fn ops(&self, ops: &mut [Op], closures: &mut Vec<CodeId>) {
        for op in ops {
            match op {
                Op::Local { local, .. }
                | Op::ChargeLocal { local, .. }
                | Op::LetEnd { local }
                | Op::WriteLocal { local, .. } => {
                    local.at = self.positions[local.slot as usize];
                }
                Op::LetBind { local, src } => {
                    let (local, src) = (self.place_ref(*local), *src);
                    *op = if self.slots[local.slot as usize].shared {
                        Op::LetBindShared { local, src }
                    } else {
                        Op::LetBind { local, src }
                    };
                }
                Op::Bind { local, src } => {
                    let (local, src) = (self.place_ref(*local), *src);
                    *op = if self.slots[local.slot as usize].shared {
                        Op::BindShared { local, src }
                    } else {
                        Op::Bind { local, src }
                    };
                }
                Op::Closure { code, .. } => closures.push(CodeId(code.0)),
                _ => {}
            }
        }
    }

    fn place_ref(&self, local: LocalRef) -> LocalRef {
        LocalRef {
            slot: local.slot,
            at: self.positions[local.slot as usize],
        }
    }

    /// Places the variables a statement's shape names, and collects the
    /// blocks it holds.
    fn form(&self, form: &mut Form, blocks: &mut Vec<BlockId>) {
        match form {
            Form::Plain | Form::ReturnVia { .. } => {}
            Form::If {
                then_block,
                else_block,
            } => blocks.extend([*then_block, *else_block]),
            Form::For {
                binding,
                body,
                else_block,
                ..
            } => {
                self.local(binding);
                blocks.push(*body);
                blocks.extend(*else_block);
            }
            Form::While {
                body, else_block, ..
            } => {
                blocks.push(*body);
                blocks.extend(*else_block);
            }
            Form::Try {
                body,
                catch,
                finally,
                ..
            } => {
                blocks.push(*body);
                if let Some((binding, block)) = catch {
                    self.local(binding);
                    blocks.push(*block);
                }
                blocks.extend(*finally);
            }
            Form::Action(action) => self.action(action),
        }
    }

    fn action(&self, action: &mut Action) {
        let (callee, atoms): (Option<&mut Callee>, &mut [Atom]) = match &mut action.kind {
            ActionKind::Call { callee, args } | ActionKind::Spawn { callee, args } => {
                (Some(callee), args)
            }
            ActionKind::Perform { args, .. } => (None, args),
            ActionKind::Sleep { duration: atom, .. }
            | ActionKind::Join(atom)
            | ActionKind::JoinMany(_, atom)
            | ActionKind::Cancel(atom) => (None, std::slice::from_mut(atom)),
            ActionKind::Yield => (None, &mut []),
        };
        if let Some(Callee::Value(var)) = callee {
            self.var(var);
        }
        for atom in atoms {
            if let Atom::Var(var) = atom {
                self.var(var);
            }
        }
    }
}
