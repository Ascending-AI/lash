//! One admitted library function and the implementation that runs it.
use super::*;
use lash_kernel_doc::NativeFunction;

pub(crate) struct Lib {
    pub(crate) index: LibId,
    pub(crate) shares_params: bool,
    pub(crate) operation: bool,
    pub(crate) id: FunctionId,
    pub(crate) definition: Arc<FunctionDefinition>,
    pub(crate) run: LibRun,
    /// How many parameters the definition has.
    pub(crate) arity: usize,
    /// The definition's charge formula, and its guard's limit.
    pub(crate) charge: Plan,
    pub(crate) limit: Option<Plan>,
    /// An unguarded native's prepared entry and its formula-derived charge:
    /// the inline fast path of the operator op that calls it
    /// (`Op::Operator`, or a nested call its entry marks). Odd layouts and
    /// arena-dependent natives use the general call.
    pub(crate) prepared: Option<(Entry, Fast)>,
}

impl Lib {
    /// A function that may answer without its body reserves that body's
    /// depth unit before offering its native the call (K-BND-002).
    /// Body-only calls reserve their ordinary frame at entry; a generated
    /// plan of one is its body, and reserves the unit at its own entry.
    pub(crate) fn operation(&self) -> bool {
        self.operation
    }

    /// What a call of `list.len` charges, whose answer on a list is that
    /// list's length: a region reads it from the arena where the list is
    /// resident, as the native's own call reads it, and charges the units
    /// of its formula, which measures nothing. `None` for every other
    /// function.
    pub(crate) fn length_read(&self) -> Option<u64> {
        (self.definition.name.as_str() == "list.len"
            && matches!(self.run, LibRun::Native { .. })
            && self.arity == 1
            && self.limit.is_none()
            && self.prepared.is_none()
            && self.charge.measurements().is_empty())
        .then(|| self.charge.evaluate(|_, _| 0))
    }
}

// Keep first-use caches out of the hot call entry's inline storage.
type LazyPlan = Box<std::sync::OnceLock<Option<Arc<PreparedPlan>>>>;

pub(crate) enum LibRun {
    /// The native implementation, and the kernel body that runs a call it
    /// declines (`NativeError::Decline`).
    Native {
        native: Arc<dyn NativeFunction>,
        body: Option<CodeId>,
        /// The same body's plan runs a decline without re-offering the native.
        fallback: Option<LazyPlan>,
    },
    /// The body's generated plan, which answers every call; the body's
    /// code stays for a frame saved inside it (FIG-6678).
    Plan {
        plan: Arc<PreparedPlan>,
        body: CodeId,
    },
    /// Callback or closure metadata is resolved on first use. A failed
    /// resolution selects the interpreted body before the call starts.
    DeferredPlan {
        body: CodeId,
        prepared: LazyPlan,
    },
    Body(CodeId),
    Machine(MachineFunction),
}

/// A generated plan as one layout runs it: its callees by their table
/// indexes, its literals as the body's constants, its fields with their
/// read hints.
pub(crate) struct PreparedPlan {
    pub(crate) arity: usize,
    pub(crate) operation: bool,
    pub(crate) plan: &'static doc::plan::HelperPlan,
    pub(crate) callees: Vec<LibId>,
    pub(crate) literals: Vec<Constant>,
    pub(crate) fields: Vec<Field>,
    pub(crate) locals: Vec<Option<Local>>,
    /// Binding needs only the shared-cell flag, not a frame-local mapping.
    pub(crate) shared: Option<Box<[bool]>>,
    pub(crate) closures: Vec<CodeId>,
    /// A segment's sites where its body's frame stands at them (FIG-6709).
    pub(crate) segment: Option<segments::Segment>,
    /// The loops of its body it runs as regions over their cursors, by
    /// the variable that holds what each walks (FIG-6780).
    pub(crate) regions: Vec<plan_regions::PlanRegion>,
}

impl PreparedPlan {
    /// The region of the loop that walks what plan variable `walked`
    /// holds, by its index.
    #[inline]
    pub(crate) fn region(&self, walked: u32) -> Option<u32> {
        self.regions
            .iter()
            .position(|region| region.walked == walked)
            .map(|at| at as u32)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a plan resolves against its body, its library and its layout"
    )]
    pub(super) fn new<'l>(
        plan: &'static doc::plan::HelperPlan,
        definition: &doc::FunctionDefinition,
        lib_of: &dyn Fn(&FunctionId) -> Option<LibId>,
        layout: Layout,
        body: CodeId,
        tables: Option<(&Tables, &Base)>,
        sites: &mut u32,
        lib: &dyn Fn(LibId) -> Option<&'l Lib>,
    ) -> Result<Self, String> {
        use doc::plan::PlanLiteral;
        let arity = definition.signature.params.len();
        let operation = definition.has_native() && definition.body().is_some();
        let callees = plan
            .callees
            .iter()
            // Installation checked that the registry holds every callee.
            .map(|callee| lib_of(callee).unwrap_or(LibId(u32::MAX)))
            .collect();
        let literals = plan
            .literals
            .iter()
            .map(|literal| {
                Constant::from(match *literal {
                    PlanLiteral::Null => doc::Literal::Null,
                    PlanLiteral::Absent => doc::Literal::Absent,
                    PlanLiteral::Bool(value) => doc::Literal::Bool(value),
                    // The generator spells each integer canonically.
                    PlanLiteral::Int(digits) => doc::Literal::Int(
                        doc::Integer::parse(digits).unwrap_or_else(|_| doc::Integer::new(0)),
                    ),
                    PlanLiteral::Float(bits) => {
                        doc::Literal::Float(doc::Float::new(f64::from_bits(bits)))
                    }
                    PlanLiteral::Text(text) => doc::Literal::Text(text.to_owned()),
                    PlanLiteral::Bytes(bytes) => doc::Literal::Bytes(doc::Bytes::new(bytes)),
                })
            })
            .collect();
        let fields = plan
            .fields
            .iter()
            .map(|name| {
                *sites += 1;
                Field::new(name, *sites - 1, layout.shape())
            })
            .collect();
        let Some((tables, base)) = tables else {
            return Ok(Self {
                arity,
                operation,
                plan,
                callees,
                literals,
                fields,
                locals: Vec::new(),
                shared: None,
                closures: Vec::new(),
                segment: None,
                regions: Vec::new(),
            });
        };
        let code = &tables.codes[(body.0 - base.codes) as usize];
        let locals: Vec<Option<Local>> = plan
            .slots
            .iter()
            .map(|local| {
                code.slots
                    .iter()
                    .enumerate()
                    .find(|(_, slot)| {
                        slot.name.as_str() == local.name && slot.declared.path == local.path
                    })
                    .map(|(index, slot)| Local {
                        slot: index as u32,
                        at: code
                            .slots_by_at
                            .iter()
                            .position(|slot| *slot == index as u32)
                            .unwrap_or(index) as u32,
                        shared: slot.shared,
                    })
            })
            .collect();
        let closures = plan
            .closures
            .iter()
            .map(|path| {
                let mut path = path.to_vec();
                path.push(0);
                tables
                    .code_at
                    .get(&Site::new(code.site.unit.clone(), path))
                    .copied()
                    .unwrap_or(CodeId(u32::MAX))
            })
            .collect();
        let segment = (!plan.sites.is_empty())
            .then(|| segments::Segment::resolve(plan, &locals, body, tables, base))
            .transpose()?;
        let regions = match &segment {
            Some(segment) if layout.regions() => {
                plan_regions::form(plan, &locals, segment, tables, base, lib)
            }
            _ => Vec::new(),
        };
        Ok(Self {
            arity,
            operation,
            plan,
            callees,
            literals,
            fields,
            shared: locals
                .iter()
                .any(|local| local.is_some_and(|local| local.shared))
                .then(|| {
                    locals
                        .iter()
                        .map(|local| local.is_some_and(|local| local.shared))
                        .collect()
                }),
            locals,
            closures,
            segment,
            regions,
        })
    }
}
