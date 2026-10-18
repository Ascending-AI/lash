//! One admitted library function and the implementation that runs it.

use super::{CodeId, Constant, Entry, Fast, Field, Layout, LibId, Local, Plan, Tables, segments};
use crate::functions::MachineFunction;
use lash_kernel_doc as doc;
use lash_kernel_doc::Site;
use lash_kernel_doc::{FunctionDefinition, FunctionId, NativeFunction};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) struct Lib {
    pub(crate) id: FunctionId,
    pub(crate) definition: Arc<FunctionDefinition>,
    pub(crate) run: LibRun,
    /// How many parameters the definition has.
    pub(crate) arity: usize,
    /// The definition's charge formula, and its guard's limit.
    pub(crate) charge: Plan,
    pub(crate) limit: Option<Plan>,
    /// An unguarded native or generated helper entry and its own formula charge:
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
        self.definition.has_native() && self.definition.body().is_some()
    }
}

pub(crate) enum LibRun {
    /// The native implementation, and the kernel body that runs a call it
    /// declines (`NativeError::Decline`).
    Native {
        native: Arc<dyn NativeFunction>,
        body: Option<CodeId>,
        /// The same body's plan runs a decline without re-offering the native.
        fallback: Option<Arc<PreparedPlan>>,
    },
    /// The body's generated plan, which answers every call; the body's
    /// code stays for a frame saved inside it (FIG-6678).
    Plan {
        plan: Arc<PreparedPlan>,
        body: CodeId,
    },
    Body(CodeId),
    Machine(MachineFunction),
}

/// A generated plan as one layout runs it: its callees by their table
/// indexes, its literals as the body's constants, its fields with their
/// read hints.
pub(crate) struct PreparedPlan {
    pub(crate) plan: &'static doc::plan::HelperPlan,
    pub(crate) callees: Vec<LibId>,
    pub(crate) literals: Vec<Constant>,
    pub(crate) fields: Vec<Field>,
    pub(crate) locals: Vec<Option<Local>>,
    pub(crate) closures: Vec<CodeId>,
    /// A segment's sites where its body's frame stands at them (FIG-6709).
    pub(crate) segment: Option<segments::Segment>,
}

impl PreparedPlan {
    pub(super) fn new(
        plan: &'static doc::plan::HelperPlan,
        lib_of: &BTreeMap<FunctionId, LibId>,
        layout: Layout,
        body: CodeId,
        tables: &Tables,
        sites: &mut u32,
    ) -> Result<Self, String> {
        use doc::plan::PlanLiteral;
        let callees = plan
            .callees
            .iter()
            // Installation checked that the registry holds every callee.
            .map(|callee| lib_of.get(callee).copied().unwrap_or(LibId(u32::MAX)))
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
                Field::new(name, *sites - 1, layout)
            })
            .collect();
        let code = &tables.codes[body.0 as usize];
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
            .then(|| segments::Segment::resolve(plan, &locals, body, tables))
            .transpose()?;
        Ok(Self {
            plan,
            callees,
            literals,
            fields,
            locals,
            closures,
            segment,
        })
    }
}
