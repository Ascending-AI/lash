//! Immutable source roles, adjacent statement exits and canonical close programs.
use super::*;
impl Compiler<'_> {
    fn source(&self) -> &Arc<Source> {
        self.placement.source()
    }
    fn slot(&self, owner: &Owner) -> Result<Slot, Refused> {
        match self.source().owner_slots()?.root(owner, TaskId::MAIN, 0) {
            Some(logical::Owner::Value { slot, .. }) => Ok(slot),
            _ => Err(Refused::Source),
        }
    }
    fn binding(
        &self,
        site: &Site,
        name: &lash_kernel_value::Name,
    ) -> Result<(Slot, bool), Refused> {
        let owners = self.source().owner_slots()?;
        let selector = super::super::root_owners::visible(&owners.scopes, site, name)
            .ok_or(Refused::Source)?;
        let Owner::Binding { declared, name } = selector else {
            return Err(Refused::Source);
        };
        let slot = self.slot(selector)?;
        if self.placement.physical(slot).is_none() {
            return Err(Refused::Source);
        }
        let shared = self.source().shared.get(&(declared.clone(), name.clone())) == Some(&true);
        Ok((slot, shared))
    }
    fn add(&mut self, at: Point, operation: Operation) -> Result<usize, Refused> {
        let interval = self.source().compile_event(&at, ExitKind::Normal)?;
        let event = interval.events().next().ok_or(Refused::Path)?.clone();
        if interval.events().len() != 1 || !event.entities.is_empty() {
            return Err(Refused::Source);
        }
        let inputs: Box<[_]> = event
            .inputs
            .iter()
            .map(|owner| self.live.get(owner).copied().ok_or(Refused::Retained))
            .collect::<Result<_, _>>()?;
        let output = event
            .output
            .as_ref()
            .map(|owner| self.slot(owner))
            .transpose()?;
        let index = self.nodes.len();
        for owner in &event.inputs {
            self.live.remove(owner);
        }
        if let Some(owner) = &event.output {
            if !matches!(output, Some(Slot::Operand(_)))
                || self.live.insert(owner.clone(), index).is_some()
            {
                return Err(Refused::Retained);
            }
            self.vacant.insert(output.ok_or(Refused::Retained)?);
        }
        self.nodes.push(Node {
            event,
            operation,
            inputs,
            output,
        });
        Ok(index)
    }
    fn expression(&mut self, expr: &doc::Expr, site: &Site) -> Result<usize, Refused> {
        let operation = match expr {
            doc::Expr::Literal(_) => match self.source().scalar_input(
                self.source().owner_slots()?.as_ref(),
                &Point {
                    site: site.clone(),
                    phase: Phase::Commit,
                },
            )? {
                crate::compile::room::region::ScalarInput::Literal(value) => {
                    Operation::Literal(value)
                }
                _ => return Err(Refused::Source),
            },
            doc::Expr::Variable(name) => {
                let (slot, shared) = self.binding(site, name)?;
                Operation::Binding { slot, shared }
            }
            doc::Expr::Member(member) => Operation::Read(match &**member {
                doc::Member::Field { target, field } => Address::Field {
                    target: self.expression(target, &site.child(0))?,
                    name: Arc::from(field.as_str()),
                },
                doc::Member::Index { target, index } => Address::Index {
                    target: self.expression(target, &site.child(0))?,
                    index: self.expression(index, &site.child(1))?,
                },
            }),
            doc::Expr::Call { function, args } => {
                if self.source().calls.get(function) != Some(&CallSource::Native(*function)) {
                    return Err(Refused::Source);
                }
                let lib = self.exe.library_lib(function).ok_or(Refused::Source)?;
                let entry = self.exe.lib(lib);
                if !matches!(entry.run, LibRun::Native { body: None, .. })
                    || entry.limit.is_some()
                    || entry.shares_params
                    || !entry.body_arity(args.len())
                    || entry.specialization.resource != CallSource::Native(*function)
                    || entry
                        .specialization
                        .prepared
                        .as_ref()
                        .is_none_or(|(entry, _)| entry.depth() != 0)
                {
                    return Err(Refused::Source);
                }
                let inputs = args
                    .iter()
                    .enumerate()
                    .map(|(i, arg)| {
                        self.expression(
                            arg,
                            &site.child(u32::try_from(i).map_err(|_| Refused::Path)?),
                        )
                    })
                    .collect::<Result<Box<[_]>, _>>()?;
                Operation::Native { lib, inputs }
            }
            _ => return Err(Refused::Source),
        };
        self.add(
            Point {
                site: site.clone(),
                phase: Phase::Commit,
            },
            operation,
        )
    }
    fn statement(&mut self, stmt: &doc::Stmt, site: &Site) -> Result<(), Refused> {
        let doc::Stmt::Assign {
            place,
            value: doc::Rhs::Expr(value),
            order,
        } = stmt
        else {
            return Err(Refused::Source);
        };
        let children = doc::Node::Stmt(stmt).children();
        let rhs = u32::try_from(children.len().checked_sub(1).ok_or(Refused::Source)?)
            .map_err(|_| Refused::Path)?;
        let value_first = *order == doc::AssignOrder::ValueFirst;
        let mut result = if value_first {
            Some(self.expression(value, &site.child(rhs))?)
        } else {
            None
        };
        let address = match place {
            doc::Place::Variable(name) => {
                let (slot, true) = self.binding(site, name)? else {
                    return Err(Refused::Source);
                };
                if !self.source().assigns_cell(site) {
                    return Err(Refused::Source);
                }
                Address::Cell(slot)
            }
            doc::Place::Member(doc::Member::Field { target, field }) => Address::Field {
                target: self.expression(target, &site.child(0))?,
                name: Arc::from(field.as_str()),
            },
            doc::Place::Member(doc::Member::Index { target, index }) => Address::Index {
                target: self.expression(target, &site.child(0))?,
                index: self.expression(index, &site.child(1))?,
            },
        };
        if result.is_none() {
            result = Some(self.expression(value, &site.child(rhs))?);
        }
        self.add(
            Point {
                site: site.clone(),
                phase: Phase::Commit,
            },
            Operation::Write {
                address,
                value: result.ok_or(Refused::Source)?,
            },
        )?;
        if !self.live.is_empty() {
            return Err(Refused::Retained);
        }
        Ok(())
    }
    fn compile_statement(
        &mut self,
        id: &StmtId,
        previous: Option<&Statement>,
    ) -> Result<Statement, Refused> {
        let exe = self.exe;
        let placement = self.placement;
        let source = Arc::clone(self.source());
        let (_, stmt, _) = exe.statement(*id);
        if exe.stmt_at(placement.code(), &stmt.site) != Some(*id) {
            return Err(Refused::Source);
        }
        if let Some(previous) = previous {
            let mut expected = previous.through.site.clone();
            let last = expected.path.last_mut().ok_or(Refused::Path)?;
            *last = last.checked_add(1).ok_or(Refused::Path)?;
            if expected != stmt.site {
                return Err(Refused::Path);
            }
        }
        let mut node = doc::Node::Block(&source.body);
        for i in stmt.site.path.iter().skip(source.site.path.len()) {
            node = *node.children().get(*i as usize).ok_or(Refused::Path)?;
        }
        let doc::Node::Stmt(ast) = node else {
            return Err(Refused::Source);
        };
        let start = self.nodes.len();
        self.statement(ast, &stmt.site)?;
        let nodes = start..self.nodes.len();
        let first = self.nodes[start].event.point.clone();
        let through = Point {
            site: stmt.site.clone(),
            phase: Phase::Commit,
        };
        let route: Vec<_> = self.nodes[nodes.clone()]
            .iter()
            .map(|node| Selection::Event {
                at: node.event.point.clone(),
                exit: ExitKind::Normal,
            })
            .collect();
        let interval = source.compile_between(&first, &through, &route, route.len())?;
        if interval
            .events()
            .ne(self.nodes[nodes.clone()].iter().map(|node| &node.event))
        {
            return Err(Refused::Source);
        }
        let charges = ChargeProgram::compile(exe, placement.code(), *id, stmt.ops)?;
        if charges.first() != &first {
            return Err(Refused::Source);
        }
        let after = source.next_after_commit(&stmt.site)?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.member-region");
        first.site.encode(&mut encoding);
        through.site.encode(&mut encoding);
        let depth = self.nodes[nodes.clone()]
            .iter()
            .filter_map(|node| match node.operation {
                Operation::Native { lib, .. } => Some(u64::from(exe.lib(lib).operation())),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        Ok(Statement {
            id: *id,
            first,
            through,
            after,
            nodes,
            charges,
            recovery: logical_digest(&encoding.into_bytes()),
            depth,
        })
    }
}
impl MemberRegion {
    #[cfg(test)]
    pub(crate) fn compile(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        statements: &[StmtId],
        footprint: &[u32],
    ) -> Result<Self, Refused> {
        if statements.len() < 2 || exe.code(placement.code()).paced {
            return Err(Refused::Path);
        }
        let mut compiler = Compiler {
            exe,
            placement: &placement,
            nodes: Vec::new(),
            live: BTreeMap::new(),
            vacant: BTreeSet::new(),
        };
        let mut compiled: Vec<Statement> = Vec::new();
        for id in statements {
            compiled.push(compiler.compile_statement(id, compiled.last())?);
        }
        for pair in compiled.windows(2) {
            if pair[0].after != pair[1].first {
                return Err(Refused::Path);
            }
        }
        let Compiler { nodes, vacant, .. } = compiler;
        let mut region = Self::from_parts(exe, placement, nodes, vacant, compiled, footprint)?;
        region.peaks = algebra::compile(exe, &region.nodes)?;
        region.verify(exe)?;
        Ok(region)
    }
    fn from_parts(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        nodes: Vec<Node>,
        vacant: BTreeSet<Slot>,
        statements: Vec<Statement>,
        footprint: &[u32],
    ) -> Result<Self, Refused> {
        let observers = ObserverScope::compile(Arc::clone(&placement), footprint, &[])?;
        Ok(Self {
            executable: exe.local(),
            code: placement.code(),
            source: Arc::clone(placement.source()),
            placement,
            observers,
            footprint: footprint.into(),
            nodes: nodes.into(),
            statements: statements.into(),
            vacant: vacant.into_iter().collect(),
            peaks: Box::default(),
        })
    }
    /// Compile the maximal adjacent assignment suffix once at its actual
    /// block entry. The footprint is lexical source placement, never a list
    /// inferred from physical extensibility or a benchmark's spelling.
    pub(crate) fn compile_tail(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        statements: &[StmtId],
    ) -> Option<Self> {
        if statements.len() < 2 || exe.code(placement.code()).paced {
            return None;
        }
        let source = Arc::clone(placement.source());
        let mut compiler = Compiler {
            exe,
            placement: &placement,
            nodes: Vec::new(),
            live: BTreeMap::new(),
            vacant: BTreeSet::new(),
        };
        let mut compiled: Vec<Statement> = Vec::new();
        for id in statements {
            let start = compiler.nodes.len();
            match compiler.compile_statement(id, compiled.last()) {
                Ok(stmt)
                    if compiled
                        .last()
                        .is_none_or(|previous| previous.after == stmt.first) =>
                {
                    compiled.push(stmt);
                }
                _ => {
                    // A failed next statement may already have staged operands.
                    // The preceding complete statement leaves no live operand.
                    compiler.nodes.truncate(start);
                    compiler.live.clear();
                    compiler.vacant = compiler
                        .nodes
                        .iter()
                        .filter_map(|node| node.output)
                        .collect();
                    break;
                }
            }
        }
        if compiled.len() < 2 {
            return None;
        }
        let first = &compiled[0].through.site;
        let owners = source.owner_slots().ok()?;
        let footprint: Vec<_> = source
            .bindings
            .iter()
            .filter_map(|binding| {
                let visible =
                    super::super::root_owners::visible(&owners.scopes, first, &binding.name)?;
                if *visible
                    != (Owner::Binding {
                        declared: binding.declared.clone(),
                        name: binding.name.clone(),
                    })
                {
                    return None;
                }
                let logical::Owner::Value { slot, .. } =
                    source.binding_root(binding.index, TaskId::MAIN, 0)?
                else {
                    return None;
                };
                placement.physical(slot)
            })
            .collect();
        let Compiler { nodes, vacant, .. } = compiler;
        let mut region =
            Self::from_parts(exe, placement, nodes, vacant, compiled, &footprint).ok()?;
        loop {
            if let Ok(peaks) = algebra::compile(exe, &region.nodes) {
                region.peaks = peaks;
                if region.verify(exe).is_ok() {
                    return Some(region);
                }
            }
            if region.statements.len() <= 2 {
                return None;
            }
            // Reuse the already compiled source events and close programs.
            // Only the discarded tail and its operand slots leave this candidate.
            let mut statements = region.statements.into_vec();
            let discarded = statements.pop()?;
            let mut nodes = region.nodes.into_vec();
            nodes.truncate(discarded.nodes.start);
            region.vacant = nodes
                .iter()
                .filter_map(|node| node.output)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            region.nodes = nodes.into();
            region.statements = statements.into();
        }
    }
    pub(crate) fn verify(&self, exe: &Executable) -> Result<(), Refused> {
        if exe.local() != self.executable
            || !Arc::ptr_eq(&self.source, &exe.code(self.code).source_room)
        {
            return Err(Refused::Source);
        }
        let mut compiler = Compiler {
            exe,
            placement: &self.placement,
            nodes: Vec::new(),
            live: BTreeMap::new(),
            vacant: BTreeSet::new(),
        };
        for stmt in &self.statements {
            let (_, actual, _) = exe.statement(stmt.id);
            let mut node = doc::Node::Block(&self.source.body);
            for index in actual.site.path.iter().skip(self.source.site.path.len()) {
                node = *node
                    .children()
                    .get(*index as usize)
                    .ok_or(Refused::Source)?;
            }
            let doc::Node::Stmt(ast) = node else {
                return Err(Refused::Source);
            };
            compiler.statement(ast, &actual.site)?;
            stmt.charges.verify(exe, self.code)?;
            if stmt.charges.first() != &stmt.first
                || self.source.next_after_commit(&stmt.through.site)? != stmt.after
            {
                return Err(Refused::Exit);
            }
            for node in &self.nodes[stmt.nodes.clone()] {
                if self.source.room.events.get(&node.event.point) != Some(&node.event) {
                    return Err(Refused::Peak);
                }
            }
        }
        if compiler.nodes.len() != self.nodes.len()
            || compiler
                .nodes
                .iter()
                .zip(self.nodes.iter())
                .any(|(actual, expected)| {
                    actual.event != expected.event
                        || !actual.operation.same(&expected.operation)
                        || actual.inputs != expected.inputs
                        || actual.output != expected.output
                })
        {
            return Err(Refused::Source);
        }
        for node in &self.nodes {
            let slot = match node.operation {
                Operation::Binding { slot, .. }
                | Operation::Write {
                    address: Address::Cell(slot),
                    ..
                } => Some(slot),
                _ => None,
            };
            if let Some(slot) = slot
                && !self
                    .placement
                    .physical(slot)
                    .is_some_and(|at| self.footprint.contains(&at))
            {
                return Err(Refused::Retained);
            }
        }
        if compiler.vacant.into_iter().collect::<Vec<_>>().as_slice() != self.vacant.as_ref() {
            return Err(Refused::Retained);
        }
        if algebra::compile(exe, &compiler.nodes)? != self.peaks {
            return Err(Refused::Peak);
        }
        Ok(())
    }
    pub(crate) fn starts_at(&self, frontier: &Frontier) -> bool {
        !frontier.proven
            && Arc::ptr_eq(&frontier.source, &self.source)
            && frontier.position.unpaid.as_ref() == Some(&self.statements[0].first)
    }
    pub(crate) fn len(&self) -> usize {
        self.statements.len()
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    pub(crate) fn placement(&self) -> &Arc<BindingPlacement> {
        &self.placement
    }
    pub(crate) fn start(&self, occurrence: Occurrence) -> Position {
        Position {
            at: self.statements[0].first.clone(),
            unpaid: Some(self.statements[0].first.clone()),
            recovery: self.statements[0].recovery,
            occurrence,
        }
    }
    #[cfg(test)]
    pub(crate) fn replace_cell_for_law(&mut self, slot: Slot) {
        let operation = self
            .nodes
            .iter_mut()
            .find_map(|node| match &mut node.operation {
                Operation::Write {
                    address: Address::Cell(target),
                    ..
                } => Some(target),
                _ => None,
            })
            .unwrap();
        *operation = slot;
    }
    #[cfg(test)]
    pub(crate) fn understate_peak_for_law(&mut self) {
        self.peaks.fill(algebra::Polynomial::default());
    }
}
