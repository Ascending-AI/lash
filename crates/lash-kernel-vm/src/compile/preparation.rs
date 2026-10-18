//! Immutable document admission and preparation, scoped to one prepared
//! library and layout. Only live document allocations can reuse admission:
//! the weak reference prevents pointer reuse and copy-on-write mutation from
//! ever selecting code for a different document. Code holds neither its
//! document nor its library, so the cache has no ownership cycle.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, Weak};

use lash_kernel_doc::{
    Document, DocumentId, FunctionId, Invalid, KernelDataLimits, validate_document_with_limits,
};

use super::*;
use crate::{OpenError, StartError};

/// The next prepared executable's process-local identity.
static NEXT_LOCAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Default)]
pub(super) struct Documents(BTreeMap<(usize, usize), Arc<Admission>>);

pub(crate) struct Admission {
    document: Weak<Document>,
    code: OnceLock<Result<Arc<DocumentCode>, Missing>>,
    identity: OnceLock<DocumentId>,
}

pub(crate) enum Refused {
    Missing(FunctionId),
    Invalid(Invalid),
}

impl From<Refused> for StartError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Missing(function) => Self::MissingFunction { function },
            Refused::Invalid(invalid) => Self::NotAdmitted(invalid),
        }
    }
}

impl From<Refused> for OpenError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Missing(function) => Self::MissingFunction { function },
            Refused::Invalid(invalid) => Self::NotAdmitted(invalid),
        }
    }
}

impl PreparedLibrary {
    /// Admission precedes compilation, preserving import's refusal order:
    /// document identity and pins are checked before missing body callees.
    pub(crate) fn admit(
        &self,
        document: &Arc<Document>,
        limits: KernelDataLimits,
    ) -> Result<Arc<Admission>, Refused> {
        let mut documents = self
            .0
            .documents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Only document depth affects structural validation. Runtime value
        // and inline limits are applied by each run's heap, not admission.
        let key = (Arc::as_ptr(document) as usize, limits.document_depth());
        if let Some(admitted) = documents.0.get(&key) {
            return Ok(Arc::clone(admitted));
        }
        // Reclaim preparation for documents no caller or machine retains.
        // Weak references keep their allocation addresses reserved until this
        // sweep, so even a dropped document can never produce a false hit.
        documents
            .0
            .retain(|_, admitted| admitted.document.strong_count() != 0);
        for function in document.manifest.functions.keys() {
            if self.registry().get(function).is_none() {
                return Err(Refused::Missing(*function));
            }
        }
        validate_document_with_limits(document, self.registry().as_ref(), limits)
            .map_err(Refused::Invalid)?;
        let admitted = Arc::new(Admission {
            document: Arc::downgrade(document),
            code: OnceLock::new(),
            identity: OnceLock::new(),
        });
        documents.0.insert(key, Arc::clone(&admitted));
        Ok(admitted)
    }
}

impl Admission {
    pub(crate) fn identity(&self, document: &Document) -> DocumentId {
        *self.identity.get_or_init(|| document.identity())
    }

    pub(crate) fn executable(
        self: &Arc<Self>,
        document: &Arc<Document>,
        library: &PreparedLibrary,
    ) -> Result<Arc<Executable>, Missing> {
        let own = self
            .code
            .get_or_init(|| compile_document(document, library).map(Arc::new));
        let own = own.as_ref().map_err(|missing| *missing)?;
        Ok(Arc::new(Executable {
            library: library.clone(),
            document: Arc::clone(document),
            own: Arc::clone(own),
            admission: Arc::clone(self),
            local: NEXT_LOCAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }))
    }
}

impl Executable {
    pub(crate) fn identity(&self) -> DocumentId {
        self.admission.identity(&self.document)
    }
}

impl PreparedLibrary {
    /// Compiles every library body in `registry` laid out as `layout`
    /// says. Every body gets a code, even one whose calls a native
    /// implementation runs: a parked run inside such a body resumes there
    /// (`K-MACH-008`). A test prepares its other layout once, and starts
    /// each run in it with [`crate::KernelMachine::start_with_layout`].
    pub(crate) fn with_layout(registry: Arc<FunctionRegistry>, layout: Layout) -> Self {
        Self::with_layout_and_charge(registry, layout, crate::Charge::Exact)
    }

    pub(super) fn with_layout_and_charge(
        registry: Arc<FunctionRegistry>,
        layout: Layout,
        charge: crate::Charge,
    ) -> Self {
        let mut shuffler = Shuffler(layout.0);
        let mut library: Vec<(FunctionId, &doc::RegisteredFunction)> = registry
            .iter()
            .map(|(function, registered)| (*function, registered))
            .collect();
        shuffler.shuffle(layout, &mut library);
        let lib_of: BTreeMap<FunctionId, LibId> = library
            .iter()
            .enumerate()
            .map(|(index, (function, _))| (*function, LibId(index as u32)))
            .collect();
        // Which functions the machine runs itself, recognised once.
        let machine: Vec<Option<MachineFunction>> = library
            .iter()
            .map(|(_, registered)| machine_function(registered.definition()))
            .collect();
        let mut units: Vec<(FunctionId, &FunctionDefinition)> = library
            .iter()
            .zip(&machine)
            .filter(|(_, machine)| machine.is_none())
            .filter(|((_, registered), _)| registered.definition().body().is_some())
            .map(|((function, registered), _)| (*function, registered.definition().as_ref()))
            .collect();
        shuffler.shuffle(layout, &mut units);
        let bodies: Vec<Runs> = library
            .iter()
            .zip(&machine)
            .map(|((_, registered), machine)| {
                let definition = registered.definition();
                if machine.is_some() || definition.body().is_none() {
                    Runs::Answers
                } else {
                    Runs::Body
                }
            })
            .collect();

        // Which functions are operators: their native's prepared entry is
        // the one `prepared` keeps below.
        let operators: Vec<Option<usize>> = library
            .iter()
            .zip(&machine)
            .map(|((_, registered), machine)| {
                let definition = registered.definition();
                let native = registered.native()?;
                let runs_native =
                    machine.is_none() && !(layout.declines() && definition.body().is_some());
                (runs_native && definition.guard.is_none() && layout.0.is_multiple_of(2))
                    .then(|| native.prepare())
                    .flatten()
                    .map(|_| definition.signature.params.len())
            })
            .collect();
        let no_bindings = NO_BINDINGS;
        let mut compiler = Compiler::new(
            layout,
            shuffler,
            Resolve {
                lib_of: &lib_of,
                listed: None,
                bodies: &bodies,
                operators: &operators,
            },
            &no_bindings,
            Base::default(),
        );
        compiler.metered = charge == crate::Charge::Exact;
        let codes: Vec<CodeId> = units.iter().map(|_| compiler.reserve()).collect();
        let mut bodies = BTreeMap::new();
        for ((function, definition), code) in units.iter().zip(codes) {
            bodies.insert(*function, code);
            if let Some(body) = definition.body() {
                let params: Vec<Name> = definition
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect();
                // Every library body is covered by its completion formula.
                // Closures it makes compile as ordinary charged code.
                let unit = Unit::Library(*function);
                compiler.unit_code(code, unit, &params, &body.block, false);
            }
        }
        let mut tables = compiler.finish();
        // A prepared library lives as long as its worker: its tables keep
        // what they hold, not what growing them reserved.
        tables.blocks.shrink_to_fit();
        tables.stmts.shrink_to_fit();
        tables.stream.ops.shrink_to_fit();
        let mut unresolved = Vec::new();
        let mut sites = tables.stream.sites;
        let libs = library
            .iter()
            .zip(machine)
            .map(|((function, registered), machine)| {
                let definition = Arc::clone(registered.definition());
                let body = bodies.get(function).copied();
                let run = match (machine, registered.native(), body) {
                    (Some(machine), _, _) => LibRun::Machine(machine),
                    (None, Some(_), Some(body)) if layout.declines() => LibRun::Body(body),
                    (None, None, Some(body)) if !layout.declines() => {
                        match registered.plan().map(|plan| {
                            PreparedPlan::new(plan, &lib_of, layout, body, &tables, &mut sites)
                        }) {
                            Some(Ok(plan)) => LibRun::Plan {
                                plan: Arc::new(plan),
                                body,
                            },
                            Some(Err(problem)) => {
                                unresolved.push(format!("{}: {problem}", definition.name));
                                LibRun::Body(body)
                            }
                            None => LibRun::Body(body),
                        }
                    }
                    (None, Some(native), body) => LibRun::Native {
                        native: Arc::clone(native),
                        fallback: body.and_then(|body| {
                            let plan = registered.plan()?;
                            match PreparedPlan::new(
                                plan, &lib_of, layout, body, &tables, &mut sites,
                            ) {
                                Ok(plan) => Some(Arc::new(plan)),
                                Err(problem) => {
                                    unresolved.push(format!("{}: {problem}", definition.name));
                                    None
                                }
                            }
                        }),
                        body,
                    },
                    // A function with no native registered has a body, and
                    // the loop above gave every such body a code.
                    (None, None, body) => LibRun::Body(body.unwrap_or(CodeId(0))),
                };
                let params = &definition.signature.params;
                let charge = Plan::new(&definition.charge, params);
                let limit = definition
                    .guard
                    .as_ref()
                    .map(|guard| Plan::new(&guard.limit, params));
                // A guarded call counts its work, which only the general
                // call does; an odd layout takes the general call always.
                let prepared = match &run {
                    LibRun::Native { native, .. }
                        if limit.is_none() && layout.0.is_multiple_of(2) =>
                    {
                        native.prepare()
                    }
                    _ => None,
                };
                let prepared =
                    prepared.map(|entry| (entry, Fast::new(&definition.charge, params, &charge)));
                Lib {
                    id: *function,
                    arity: params.len(),
                    charge,
                    limit,
                    prepared,
                    definition,
                    run,
                }
            })
            .collect();
        let missing = missing(&registry);
        Self(Arc::new(Prepared {
            charge,
            alternative: Default::default(),
            registry,
            layout,
            tables,
            libs,
            lib_of,
            missing,
            unresolved,
            sites,
            documents: Default::default(),
        }))
    }
}
