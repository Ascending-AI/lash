//! Process-local helper preparation. A syntax census reserves disjoint table
//! ranges without lowering code. First use fills only one helper's tables;
//! saved sites still select the same body under a fresh library and layout.
use std::sync::{Arc, OnceLock};

use super::*;

pub(super) struct Body {
    function: FunctionId,
    pub(super) base: Base,
    tables: OnceLock<Tables>,
    closures: bool,
}

#[derive(Default)]
pub(super) struct LibraryTables {
    pub(super) bodies: Vec<Body>,
    pub(super) body_of: BTreeMap<FunctionId, usize>,
    // Dense owner indexes preserve constant-time code/statement lookup.
    pub(super) codes: Vec<usize>,
    pub(super) blocks: Vec<usize>,
    pub(super) stmts: Vec<usize>,
    pub(super) occurrence_at: BTreeMap<Site, usize>,
    pub(super) occurrence_keys: Box<[Vec<u8>]>,
}

impl LibraryTables {
    pub(super) fn compiled(&self) -> usize {
        self.bodies
            .iter()
            .filter(|body| body.tables.get().is_some())
            .count()
    }

    #[inline]
    pub(super) fn tables<'p>(&'p self, prepared: &'p Prepared, at: usize) -> &'p Tables {
        match self.bodies[at].tables.get() {
            Some(tables) => tables,
            None => self.compile(prepared, at),
        }
    }

    #[cold]
    fn compile<'p>(&'p self, prepared: &'p Prepared, at: usize) -> &'p Tables {
        let body = &self.bodies[at];
        body.tables.get_or_init(|| {
            let registered = prepared
                .registry
                .get(&body.function)
                .unwrap_or_else(|| unreachable!("indexed registry function"));
            #[cfg(feature = "perf-witness")]
            crate::perf_witness::preparation(crate::perf_witness::Preparation::Begin {
                function: body.function,
                callback: registered.plan().is_some_and(|plan| !plan.sites.is_empty()),
            });
            let definition = registered.definition();
            let block = &definition
                .body()
                .unwrap_or_else(|| unreachable!("indexed library body"))
                .block;
            let params = definition
                .signature
                .params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>();
            let no_bindings = NO_BINDINGS;
            let mut compiler = Compiler::new(
                prepared.layout.shape(),
                Shuffler(prepared.layout.shape().0),
                Resolve {
                    lib_of: &prepared.lib_of,
                    listed: None,
                    bodies: &prepared.runs,
                    operators: &prepared.operators,
                },
                &no_bindings,
                body.base,
            );
            compiler.metered = prepared.charge == crate::Charge::Exact;
            let code = compiler.reserve();
            compiler.unit_code(code, Unit::Library(body.function), &params, block, false);
            let mut tables = compiler.finish();
            // The library occurrence census is independent of which helper
            // happened to compile first and of the layout's code order.
            for stmt in &mut tables.stmts {
                if let Form::Action(action) = &mut stmt.form {
                    action.occurrence = self
                        .occurrence_at
                        .get(&action.site)
                        .copied()
                        .unwrap_or(usize::MAX);
                }
            }
            for action in &mut tables.stream.nested {
                action.occurrence = self
                    .occurrence_at
                    .get(&action.site)
                    .copied()
                    .unwrap_or(usize::MAX);
            }
            debug_assert_eq!(
                tables.codes.len(),
                self.codes.iter().filter(|owner| **owner == at).count()
            );
            debug_assert_eq!(
                tables.blocks.len(),
                self.blocks.iter().filter(|owner| **owner == at).count()
            );
            debug_assert_eq!(
                tables.stmts.len(),
                self.stmts.iter().filter(|owner| **owner == at).count()
            );
            #[cfg(feature = "perf-witness")]
            crate::perf_witness::preparation(crate::perf_witness::Preparation::End);
            tables
        })
    }

    #[inline(never)]
    pub(super) fn code<'p>(&'p self, prepared: &'p Prepared, id: CodeId) -> Option<&'p Code> {
        let owner = *self.codes.get(id.0 as usize)?;
        self.tables(prepared, owner)
            .codes
            .get((id.0 - self.bodies[owner].base.codes) as usize)
    }

    #[inline(never)]
    pub(super) fn block<'p>(&'p self, prepared: &'p Prepared, id: BlockId) -> Option<&'p Block> {
        let owner = *self.blocks.get(id.0 as usize)?;
        self.tables(prepared, owner)
            .blocks
            .get((id.0 - self.bodies[owner].base.blocks) as usize)
    }

    #[inline(never)]
    pub(super) fn stmt<'p>(&'p self, prepared: &'p Prepared, id: StmtId) -> Option<&'p Stmt> {
        let owner = *self.stmts.get(id.0 as usize)?;
        self.tables(prepared, owner)
            .stmts
            .get((id.0 - self.bodies[owner].base.stmts) as usize)
    }

    pub(super) fn verify_stream(&self, prepared: &Prepared) -> Result<usize, KirError> {
        let mut checked = 0;
        for (index, body) in self.bodies.iter().enumerate() {
            checked += self.tables(prepared, index).verify_stream(
                &body.base,
                self.codes.len(),
                self.blocks.len(),
                prepared.libs.len(),
            )?;
        }
        Ok(checked)
    }

    fn reserve(&mut self, function: FunctionId, block: &doc::Block, sites: &mut u32) {
        let owner = self.bodies.len();
        let base = Base {
            codes: self.codes.len() as u32,
            blocks: self.blocks.len() as u32,
            stmts: self.stmts.len() as u32,
            occurrences: 0,
            sites: *sites,
        };
        self.codes.push(owner);
        let mut closures = false;
        let mut pending = vec![(
            doc::Node::Block(block),
            Site::new(Unit::Library(function), []),
        )];
        while let Some((node, site)) = pending.pop() {
            // Match the property cache sites lowering allocates, without
            // constructing any executable tables or constants.
            *sites += match node {
                doc::Node::Expr(doc::Expr::Member(_))
                | doc::Node::Stmt(
                    doc::Stmt::Assign {
                        place: doc::Place::Member(doc::Member::Field { .. }),
                        ..
                    }
                    | doc::Stmt::Remove {
                        member: doc::Member::Field { .. },
                    },
                ) => 1,
                _ => 0,
            };
            match node {
                doc::Node::Block(_) => self.blocks.push(owner),
                doc::Node::Stmt(_) => self.stmts.push(owner),
                doc::Node::Expr(doc::Expr::Closure(_)) => {
                    self.codes.push(owner);
                    closures = true;
                }
                _ => {}
            }
            let counted = match node {
                doc::Node::Action(
                    doc::Action::Perform { .. }
                    | doc::Action::Sleep { .. }
                    | doc::Action::Spawn { .. },
                ) => true,
                doc::Node::Expr(doc::Expr::Act(act)) => matches!(
                    act.kind,
                    doc::ActKind::Perform { .. }
                        | doc::ActKind::Sleep { .. }
                        | doc::ActKind::Spawn(_)
                ),
                _ => false,
            };
            if counted {
                self.occurrence_at.insert(site.clone(), 0);
            }
            for (index, child) in node.children().into_iter().enumerate() {
                pending.push((child, site.child(index as u32)));
            }
        }
        self.body_of.insert(function, owner);
        self.bodies.push(Body {
            function,
            base,
            tables: OnceLock::new(),
            closures,
        });
    }
}

impl PreparedLibrary {
    /// Indexes a registry without compiling any body. A body owns its table
    /// range regardless of when it is first used, including on cold restore.
    pub(crate) fn with_layout(registry: Arc<FunctionRegistry>, layout: Layout) -> Self {
        Self::with_layout_and_charge(registry, layout, crate::Charge::Exact)
    }

    pub(super) fn with_layout_and_charge(
        registry: Arc<FunctionRegistry>,
        stated: Layout,
        charge: crate::Charge,
    ) -> Self {
        let layout = stated.shape();
        let mut functions: Vec<_> = registry.iter().map(|(id, _)| *id).collect();
        Shuffler(layout.0).shuffle(layout, &mut functions);
        let lib_of = functions
            .iter()
            .enumerate()
            .map(|(at, id)| (*id, LibId(at as u32)))
            .collect();
        let mut tables = LibraryTables::default();
        let mut sites = 0;
        let mut runs = Vec::new();
        let mut operators = Vec::new();
        let mut entries = Vec::new();
        for function in &functions {
            let registered = registry
                .get(function)
                .unwrap_or_else(|| unreachable!("registry index"));
            let definition = registered.definition();
            let machine = machine_function(definition).is_some();
            if !machine && let Some(body) = definition.body() {
                tables.reserve(*function, &body.block, &mut sites);
            }
            runs.push(if !machine && definition.body().is_some() {
                Runs::Body
            } else {
                Runs::Answers
            });
            let entry = if machine || definition.guard.is_some() || !layout.0.is_multiple_of(2) {
                None
            } else {
                registered
                    .plan()
                    .and_then(|plan| plan.template.as_ref())
                    .and_then(|template| Entry::template(&registry, template))
                    .or_else(|| registered.native()?.prepare().map(Entry::Native))
            };
            operators.push(entry.as_ref().map(|_| definition.signature.params.len()));
            entries.push(entry);
        }
        let by_key: BTreeMap<_, _> = tables
            .occurrence_at
            .keys()
            .map(|site| {
                (
                    lash_kernel_state::kernel_internals::metadata::metadata_key(site),
                    site.clone(),
                )
            })
            .collect();
        tables.occurrence_keys = by_key.keys().cloned().collect();
        tables.occurrence_at = by_key
            .into_values()
            .enumerate()
            .map(|(at, site)| (site, at))
            .collect();
        // Plan fields have their own cache sites, separate from body and
        // document sites, even if no interpreted body ever runs.
        let plan_sites = functions
            .iter()
            .map(|function| {
                let start = sites;
                if let Some(plan) = registry.get(function).and_then(|r| r.plan()) {
                    sites += plan.fields.len() as u32;
                }
                start
            })
            .collect();
        let missing = missing(&registry);
        let mut prepared = Prepared {
            charge,
            alternative: Default::default(),
            registry,
            layout: stated,
            tables,
            functions,
            libs: Vec::new(),
            lib_of,
            missing,
            sites,
            documents: Default::default(),
            runs,
            operators,
            plan_sites,
            shared_params: Vec::new(),
        };
        let (libs, shared_params) = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| prepared.prepare_lib(LibId(index as u32), entry))
            .unzip();
        prepared.libs = libs;
        prepared.shared_params = shared_params;
        Self(Arc::new(prepared))
    }
}

impl Prepared {
    #[inline]
    pub(super) fn lib(&self, id: LibId) -> &Lib {
        &self.libs[id.0 as usize]
    }

    fn prepare_lib(&self, id: LibId, entry: Option<Entry>) -> (Lib, Vec<usize>) {
        let index = id.0 as usize;
        let function = self.functions[index];
        let registered = self
            .registry
            .get(&function)
            .unwrap_or_else(|| unreachable!("registry index"));
        let definition = Arc::clone(registered.definition());
        let body_slot = self
            .tables
            .body_of
            .get(&function)
            .map(|at| (*at, &self.tables.bodies[*at]));
        let body = body_slot.map(|(_, slot)| CodeId(slot.base.codes));
        let deferred = registered
            .plan()
            .is_some_and(|plan| !plan.sites.is_empty() || !plan.closures.is_empty());
        let plan = if registered.native().is_none() && !self.layout.declines() && !deferred {
            self.prepare_plan(index)
        } else {
            None
        };
        let run = match (machine_function(&definition), registered.native(), body) {
            (Some(machine), _, _) => LibRun::Machine(machine),
            (None, Some(_), Some(body)) if self.layout.declines() => LibRun::Body(body),
            (None, Some(native), body) => LibRun::Native {
                native: Arc::clone(native),
                body,
                fallback: body.map(|_| Box::default()),
            },
            (None, None, Some(body)) if deferred && !self.layout.declines() => {
                LibRun::DeferredPlan {
                    body,
                    prepared: Box::default(),
                }
            }
            (None, None, Some(body)) => match plan {
                Some(plan) => LibRun::Plan { plan, body },
                None => LibRun::Body(body),
            },
            (None, None, None) => unreachable!("registered functions have an implementation"),
        };
        let params = &definition.signature.params;
        let charge = Plan::new(&definition.charge, params);
        let limit = definition
            .guard
            .as_ref()
            .map(|guard| Plan::new(&guard.limit, params));
        let prepared = entry.map(|entry| {
            let fast = Fast::new(&definition.charge, params, &charge, entry.immediate());
            (entry, fast)
        });
        let shared_params = if body_slot.is_some_and(|(_, slot)| slot.closures) {
            parameters::shared(&definition)
        } else {
            Vec::new()
        };
        (
            Lib {
                index: id,
                shares_params: !shared_params.is_empty(),
                operation: definition.has_native() && definition.body().is_some(),
                id: function,
                arity: params.len(),
                definition,
                run,
                charge,
                limit,
                prepared,
            },
            shared_params,
        )
    }
}

impl Prepared {
    fn prepare_plan(&self, index: usize) -> Option<Arc<PreparedPlan>> {
        self.checked_plan(index).ok().flatten()
    }

    pub(super) fn checked_plan(&self, index: usize) -> Result<Option<Arc<PreparedPlan>>, String> {
        let function = self.functions[index];
        let Some(registered) = self.registry.get(&function) else {
            return Ok(None);
        };
        let Some(plan) = registered.plan() else {
            return Ok(None);
        };
        let Some(&at) = self.tables.body_of.get(&function) else {
            return Ok(None);
        };
        let slot = &self.tables.bodies[at];
        let tables = (!plan.sites.is_empty() || !plan.closures.is_empty())
            .then(|| (self.tables.tables(self, at), &slot.base));
        let mut sites = self.plan_sites[index];
        let lib = |lib: LibId| self.libs.get(lib.0 as usize);
        PreparedPlan::new(
            plan,
            registered.definition(),
            &self.lib_of,
            self.layout,
            CodeId(slot.base.codes),
            tables,
            &mut sites,
            &lib,
        )
        .map(|plan| Some(Arc::new(plan)))
    }

    #[inline]
    pub(super) fn fallback(&self, id: LibId) -> Option<&Arc<PreparedPlan>> {
        let fallback = match &self.lib(id).run {
            LibRun::Native { fallback, .. } => fallback.as_deref()?,
            LibRun::DeferredPlan { prepared, .. } => prepared.as_ref(),
            _ => return None,
        };
        match fallback.get() {
            Some(plan) => plan.as_ref(),
            None => self.compile_fallback(fallback, id),
        }
    }

    #[cold]
    #[inline(never)]
    fn compile_fallback<'p>(
        &'p self,
        fallback: &'p OnceLock<Option<Arc<PreparedPlan>>>,
        id: LibId,
    ) -> Option<&'p Arc<PreparedPlan>> {
        fallback
            .get_or_init(|| self.prepare_plan(id.0 as usize))
            .as_ref()
    }
}
