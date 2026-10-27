//! Process-local helper preparation. A syntax census reserves disjoint table
//! ranges without lowering code. First use fills only one helper's tables;
//! saved sites still select the same body under a fresh library and layout.
use doc::FrameArray;
use std::sync::{Arc, OnceLock};

use super::*;

pub(super) struct Body {
    pub(super) function: FunctionId,
    pub(super) base: Base,
    pub(super) tables: OnceLock<Tables>,
    pub(super) closures: bool,
    pub(super) programs: OnceLock<islands::Islands>,
}

#[derive(Default)]
pub(super) struct LibraryTables {
    pub(super) bodies: FrameArray<Body>,
    /// Each library function's body, by library id: `NO_BODY` for one
    /// with none.
    pub(super) body_of: FrameArray<u32>,
    // Dense owner indexes preserve constant-time code/statement lookup.
    pub(super) codes: FrameArray<usize>,
    pub(super) blocks: FrameArray<usize>,
    pub(super) stmts: FrameArray<usize>,
    pub(super) occurrence_at: BTreeMap<Site, usize>,
    pub(super) occurrence_keys: Box<[Vec<u8>]>,
    pub(super) occurrence_units: Box<[Unit]>,
}

/// A library function with no body to compile.
pub(super) const NO_BODY: u32 = u32::MAX;

impl LibraryTables {
    /// The body of library function `lib`, when it has one.
    pub(super) fn body_of(&self, lib: LibId) -> Option<usize> {
        match self.body_of.get(lib.0 as usize) {
            Some(&body) if body != NO_BODY => Some(body as usize),
            _ => None,
        }
    }

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
                prepared.layout,
                Shuffler(prepared.layout.0),
                Resolve {
                    helpers: None,
                    library: prepared,
                    listed: None,
                },
                &no_bindings,
                body.base,
            );
            compiler.metered = prepared.charge == crate::Charge::Exact;
            let code = compiler.reserve();
            compiler.unit_code(
                code,
                Unit::Library(body.function),
                &params,
                block,
                matches!(
                    definition.implementation,
                    doc::Implementation::Guest {
                        metering: doc::Metering::Statements,
                        ..
                    }
                ),
            );
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
            let programs = islands::form(
                &mut tables,
                prepared,
                (body.base.codes, body.base.blocks, body.base.stmts),
                crate::costs::Costs::of(
                    doc::KernelVersion::of(registered.header().kernel)
                        .unwrap_or(doc::KernelVersion::NEWEST),
                )
                .loop_test,
            );
            let _ = body.programs.set(programs);
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

    /// Reserves `function`'s table ranges as its body's census states them.
    fn reserve(&mut self, function: FunctionId, census: &doc::BodyCensus, sites: &mut u32) {
        let owner = self.bodies.len();
        let base = Base {
            codes: self.codes.len() as u32,
            blocks: self.blocks.len() as u32,
            stmts: self.stmts.len() as u32,
            occurrences: 0,
            sites: *sites,
        };
        *sites += census.sites;
        self.codes
            .resize(self.codes.len() + census.codes as usize, owner);
        self.blocks
            .resize(self.blocks.len() + census.blocks as usize, owner);
        self.stmts
            .resize(self.stmts.len() + census.stmts as usize, owner);
        for site in &census.occurrences {
            self.occurrence_at.insert(site.clone(), 0);
        }
        self.body_of.push(owner as u32);
        self.bodies.push(Body {
            function,
            base,
            tables: OnceLock::new(),
            closures: census.closures,
            programs: OnceLock::new(),
        });
    }
}

/// What preparation reserves for a library function's body before it
/// compiles it ([`doc::BodyCensus`]): none for a function with no body or
/// one the machine runs itself. A build writes it into its release image
/// (FIG-6843), and a process computes it only for a function registered one
/// by one.
pub fn body_census(
    definition: &FunctionDefinition,
    catalog: &dyn doc::FunctionCatalog,
) -> Option<doc::BodyCensus> {
    if machine_function(definition).is_some() {
        return None;
    }
    let body = definition.body()?;
    let mut census = doc::BodyCensus {
        codes: 1,
        ..doc::BodyCensus::default()
    };
    // Each pending node retains its path in its own unit. Visiting a
    // closure's body must not replace the path of a pending parent sibling.
    let unit = Unit::Library(definition.identity());
    let charged = matches!(
        definition.implementation,
        doc::Implementation::Guest {
            metering: doc::Metering::Statements,
            ..
        }
    );
    let mut pending = vec![(
        doc::Node::Block(&body.block),
        Vec::<u32>::new(),
        unit,
        charged,
    )];
    while let Some((node, path, unit, charged)) = pending.pop() {
        // Match the property cache sites lowering allocates, without
        // constructing any executable tables or constants.
        census.sites += match node {
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
            doc::Node::Block(_) => census.blocks += 1,
            doc::Node::Stmt(_) => census.stmts += 1,
            doc::Node::Expr(doc::Expr::Closure(closure)) => {
                census.codes += 1;
                census.closures = true;
                if let Some(definition) = catalog.definition(&closure.function)
                    && let Some(body) = definition.body()
                {
                    pending.push((
                        doc::Node::Block(&body.block),
                        Vec::new(),
                        Unit::Library(closure.function),
                        true,
                    ));
                }
            }
            _ => {}
        }
        let counted = match node {
            doc::Node::Action(action) => {
                charged
                    || matches!(
                        action,
                        doc::Action::Perform { .. } | doc::Action::Spawn { .. }
                    )
            }
            doc::Node::Expr(doc::Expr::Act(act)) => {
                charged
                    || matches!(
                        act.kind,
                        doc::ActKind::Perform { .. } | doc::ActKind::Spawn(_)
                    )
            }
            _ => false,
        };
        if counted {
            census
                .occurrences
                .push(Site::new(unit.clone(), path.clone()));
        }
        for (index, child) in node.children().into_iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(index as u32);
            pending.push((child, child_path, unit.clone(), charged));
        }
    }
    Some(census)
}

/// A registered function's census: its build's, for a released function,
/// and computed from its definition otherwise.
pub(super) fn census_of(
    registered: &doc::RegisteredFunction,
    catalog: &dyn doc::FunctionCatalog,
) -> Option<doc::BodyCensus> {
    if registered.released() {
        registered.census()
    } else {
        body_census(registered.definition(), catalog)
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
        let layout = stated;
        let mut order: Vec<usize> = (0..registry.len()).collect();
        Shuffler(layout.0).shuffle(layout, &mut order);
        let registered: Vec<_> = registry.iter().collect();
        let functions: Vec<FunctionId> = order.iter().map(|at| *registered[*at].0).collect();
        // Library ids by registry position, when the layout permutes them.
        let lib_at = (layout.0 != 0).then(|| {
            let mut lib_at = vec![0; order.len()].into_boxed_slice();
            for (lib, at) in order.iter().enumerate() {
                lib_at[*at] = lib as u32;
            }
            lib_at
        });
        let mut tables = LibraryTables {
            bodies: FrameArray::with_capacity(order.len()),
            body_of: FrameArray::with_capacity(order.len()),
            ..LibraryTables::default()
        };
        let mut sites = 0;
        let mut runs = Vec::with_capacity(order.len());
        for at in &order {
            let (function, registered) = registered[*at];
            let census = census_of(registered, registry.as_ref());
            match &census {
                Some(census) => tables.reserve(*function, census, &mut sites),
                None => tables.body_of.push(NO_BODY),
            }
            runs.push(if census.is_some() {
                Runs::Body
            } else {
                Runs::Answers
            });
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
        tables.occurrence_units = by_key.values().map(|site| site.unit.clone()).collect();
        tables.occurrence_at = by_key
            .into_values()
            .enumerate()
            .map(|(at, site)| (site, at))
            .collect();
        // Only a body that names a function the registry lacks can reach
        // one.
        let missing = if registry.holds_every_callee() {
            BTreeMap::new()
        } else {
            missing(&registry)
        };
        drop(registered);
        let libs = functions.iter().map(|_| OnceLock::new()).collect();
        Self(
            Arc::new(Prepared {
                charge,
                alternative: Default::default(),
                registry,
                layout: stated,
                tables,
                functions: functions.into(),
                lib_at,
                libs,
                missing,
                sites,
                documents: Default::default(),
                runs: runs.into(),
                frame_index: None,
            }),
            doc::BuildHelpers::empty(),
        )
    }
}

impl Prepared {
    /// Library function `id`, prepared when first asked for: preparation
    /// never reads a function no run calls.
    #[inline]
    pub(super) fn lib(&self, id: LibId) -> &Lib {
        &self.slot(id).lib
    }

    #[inline]
    pub(super) fn slot(&self, id: LibId) -> &LibSlot {
        let slot = &self.libs[id.0 as usize];
        match slot.get() {
            Some(slot) => slot,
            None => self.prepare_slot(slot, id),
        }
    }

    #[cold]
    #[inline(never)]
    fn prepare_slot<'p>(&'p self, slot: &'p OnceLock<LibSlot>, id: LibId) -> &'p LibSlot {
        slot.get_or_init(|| self.prepare_lib(id))
    }

    /// Library function `id`, when the library numbers it.
    pub(super) fn get_lib(&self, id: LibId) -> Option<&Lib> {
        ((id.0 as usize) < self.libs.len()).then(|| self.lib(id))
    }

    /// Library function `function`'s id in this layout, reached or not.
    pub(super) fn lib_of(&self, function: &FunctionId) -> Option<LibId> {
        let at = self.registry.position(function)?;
        Some(LibId(match &self.lib_at {
            Some(lib_at) => lib_at[at],
            None => at as u32,
        }))
    }

    /// The arity an operator call of `lib` takes, when its native states a
    /// prepared entry this layout calls (`Op::Operator`).
    pub(super) fn operator(&self, lib: LibId) -> Option<usize> {
        if lib.0 as usize >= self.libs.len() {
            return None;
        }
        let lib = self.lib(lib);
        lib.specialization.prepared.as_ref().map(|_| lib.arity)
    }

    fn prepare_lib(&self, id: LibId) -> LibSlot {
        let index = id.0 as usize;
        let function = self.functions[index];
        let registered = self
            .registry
            .get(&function)
            .unwrap_or_else(|| unreachable!("registry index"));
        // Its header alone: a body is decoded only to compile it, and a
        // function the machine runs itself only by one of its names
        // (FIG-6931).
        let header = Arc::clone(registered.header());
        let machine = if machine_name(header.name.as_str()) {
            machine_function(registered.definition())
        } else {
            None
        };
        let native = registered
            .native()
            .map_or_else(Default::default, |native| native.specialization());
        let entry =
            if machine.is_some() || header.guard.is_some() || !self.layout.0.is_multiple_of(2) {
                None
            } else {
                // A native's prepared entry is its own implementation; a
                // generated template supplies an entry only where none exists.
                native.immediate.map(Entry::Native).or_else(|| {
                    let template = registered.plan()?.template.as_ref()?;
                    Entry::template(&self.registry, template)
                })
            };
        let body_slot = self
            .tables
            .body_of(id)
            .map(|at| (at, &self.tables.bodies[at]));
        let body = body_slot.map(|(_, slot)| CodeId(slot.base.codes));
        let run = match (machine, registered.native(), body) {
            (Some(machine), _, _) => LibRun::Machine(machine),
            (None, Some(_), Some(body)) if self.layout.declines() => LibRun::Body(body),
            (None, Some(native), body) => LibRun::Native {
                native: Arc::clone(native),
                body,
            },
            (None, None, Some(body)) => LibRun::Body(body),
            (None, None, None) => LibRun::Declaration,
        };
        let params = &header.signature.params;
        let formula = header
            .charge()
            .cloned()
            .unwrap_or(doc::Formula::Constant(0));
        let charge = Plan::new(&formula, params);
        let limit = header
            .guard
            .as_ref()
            .map(|guard| Plan::new(&guard.limit, params));
        let prepared = entry.map(|entry| {
            let fast = Fast::new(&formula, params, &charge, entry.immediate());
            (entry, fast)
        });
        let shared_params = if body_slot.is_some_and(|(_, slot)| slot.closures) {
            parameters::shared(registered.definition(), self.registry.as_ref())
        } else {
            Vec::new()
        };
        LibSlot {
            lib: Lib {
                index: id,
                shares_params: !shared_params.is_empty(),
                operation: header.implementation == lash_kernel_doc::ImplementationKind::Both,
                arity: params.len(),
                header,
                run,
                charge,
                limit,
                specialization: super::library::Specialization {
                    function,
                    prepared,
                    native,
                },
            },
            shared_params,
        }
    }
}

/// A library function as one preparation runs it, with the parameters its
/// body's closures share, which admission reads outside the hot call table.
pub(super) struct LibSlot {
    pub(super) lib: Lib,
    pub(super) shared_params: Vec<usize>,
}
