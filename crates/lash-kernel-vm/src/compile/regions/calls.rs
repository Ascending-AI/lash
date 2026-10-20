//! An allocating plan's operator call sites (FIG-6965): each operator
//! whose function has a prepared entry is resolved once, where its region
//! compiles. Its site holds the entry's immediate path and natives, its
//! decline (a template's heap paths over their resolved general callees,
//! or the function's general call), its scalar charge table and its
//! immediate formula, with what its constant and fresh operands measure
//! folded in ([`SiteFormula`]). A call evaluates its operands in place and
//! answers as [`Cx::operated`] answers the entry's answer: the same values,
//! declines, misses and units, in the same order.

use lash_kernel_doc::plan::{HeapTemplate, ImmediateEntry};
use lash_kernel_doc::{PreparedAnswer, PreparedNative, Value};

use super::super::{Entry, LibId, LibRun, ScalarTable, SiteFormula};
use super::cx::Cx;
use super::nodes::{Fresh, Leaf, Libs, Node};
use crate::machine::SlotState;

/// What a site's entry is.
enum Answers {
    /// A native's prepared entry.
    Native(PreparedNative),
    /// A generated template's immediate paths, when it has any.
    Template(Option<ImmediateEntry>),
}

/// What a site's call does where its entry declines.
enum Declined {
    /// The function's general call: an installed native answers first,
    /// and a native entry, or a template with no heap paths or a missing
    /// callee, leaves the call to it.
    General,
    /// The template's heap paths, over their general callees.
    Heap(&'static HeapTemplate, Box<[LibId]>),
}

/// An operator site of an allocating plan: library function `id`'s
/// prepared entry, resolved.
pub(super) struct Site {
    id: LibId,
    answers: Answers,
    /// The template's resolved prepared natives.
    natives: Box<[PreparedNative]>,
    declined: Declined,
    /// The scalar charge table, where the entry answers immediates.
    table: Option<ScalarTable>,
    formula: Option<SiteFormula>,
}

impl Site {
    /// Operator `id`'s site in an allocating plan, its operands at
    /// `known` constants and at `fresh` constructions of no members: `None`
    /// where its function has no prepared entry.
    pub(super) fn of(
        libs: &Libs<'_>,
        id: LibId,
        known: &[Option<Value>],
        fresh: Fresh,
    ) -> Option<Self> {
        let function = libs.exe.lib(id);
        let (entry, fast) = function.prepared.as_ref()?;
        let installed = matches!(function.run, LibRun::Native { .. });
        let (answers, natives, declined) = match entry {
            Entry::Native(native) => (Answers::Native(*native), Box::default(), Declined::General),
            Entry::Template {
                template, natives, ..
            } => {
                let heap = template.heap.as_ref().filter(|_| !installed);
                let declined = heap
                    .and_then(|heap| {
                        let callees = heap
                            .callees
                            .iter()
                            .map(|callee| libs.exe.library_lib(callee))
                            .collect::<Option<Box<[_]>>>()?;
                        Some(Declined::Heap(heap, callees))
                    })
                    .unwrap_or(Declined::General);
                (Answers::Template(template.entry), natives.clone(), declined)
            }
        };
        Some(Self {
            id,
            answers,
            natives,
            declined,
            table: fast.scalar().cloned(),
            formula: fast.at_site(known, fresh),
        })
    }

    /// The call on `values`, one per operand: the entry's answer charged,
    /// or what its decline leaves.
    #[inline(always)]
    pub(super) fn call(&self, values: &[Value], cx: &mut Cx<'_>) -> Option<Value> {
        let answer = match &self.answers {
            Answers::Native(native) => native.answer(values, cx.charging),
            Answers::Template(entry) => (*entry)
                .and_then(|entry| entry(values, &self.natives))
                .map(|value| PreparedAnswer {
                    value,
                    scalar_sizes: if cx.charging {
                        super::super::templates::scalar_sizes(values, value)
                    } else {
                        None
                    },
                }),
        };
        match answer {
            Some(answer) => self.charged(values, answer, cx),
            None => self.declined(values, cx),
        }
    }

    /// The entry's `answer` on `values`, charged as [`Cx::operated`]
    /// charges it: by the scalar table where its guard certified the sizes,
    /// and otherwise by its formula.
    #[inline(always)]
    pub(super) fn charged(
        &self,
        values: &[Value],
        answer: PreparedAnswer,
        cx: &mut Cx<'_>,
    ) -> Option<Value> {
        if cx.charging {
            let certified = answer
                .scalar_sizes
                .zip(self.table.as_ref())
                .and_then(|(mask, table)| table.units(mask));
            let units = match certified {
                Some(units) => units,
                None => self.units(values, &answer.value, cx)?,
            };
            cx.meter.measured(units);
        }
        Some(answer.value)
    }

    /// What the call of `values` that gave `result` charges by the
    /// entry's formula: from the values themselves where each carries its
    /// measurements, the site's fresh operands included while no write is
    /// held, and as the reference measures them otherwise.
    #[inline(always)]
    fn units(&self, values: &[Value], result: &Value, cx: &Cx<'_>) -> Option<u64> {
        let held = cx.held.is_some();
        if let Some(units) = self
            .formula
            .as_ref()
            .and_then(|formula| formula.units(values, result, held))
        {
            return Some(units);
        }
        let function = cx.exe.lib(self.id);
        let (_, fast) = function.prepared.as_ref()?;
        cx.charge_of(function, fast, values, result)
    }

    /// The call on `values` where the entry declined: the template's heap
    /// paths, charged as the entry's answer is, or the function's general
    /// call.
    #[inline(always)]
    fn declined(&self, values: &[Value], cx: &mut Cx<'_>) -> Option<Value> {
        let Declined::Heap(heap, callees) = &self.declined else {
            let function = cx.exe.lib(self.id);
            return cx.windowed(values, |cx, args| cx.general(function, args));
        };
        let value = cx.heap_paths(heap, callees, &self.natives, values)?;
        if cx.charging {
            let units = self.units(values, &value, cx)?;
            cx.meter.measured(units);
        }
        Some(value)
    }
}

/// The most operands a site evaluates in place: as many as a template's
/// heap paths take.
pub(super) const IN_PLACE: usize = 8;

/// The compiled call of operator `id` on `args` at its site, its operands
/// read in place where they are variables or constants: `None` where it
/// has no prepared entry or more operands than [`IN_PLACE`].
pub(super) fn operator<A>(
    libs: &Libs<'_>,
    id: LibId,
    args: &[A],
    mut leaf: impl FnMut(&A) -> Option<Leaf>,
    fresh: Fresh,
) -> Option<Node> {
    if !libs.general || args.len() > IN_PLACE {
        return None;
    }
    libs.exe.lib(id).prepared.as_ref()?;
    let leaves = args.iter().map(&mut leaf).collect::<Option<Vec<_>>>()?;
    let known = leaves.iter().map(Leaf::constant).collect::<Vec<_>>();
    let site = Site::of(libs, id, &known, fresh)?;
    Some(match leaves.len() {
        0 => called::<0>(site, leaves)?,
        1 => called::<1>(site, leaves)?,
        2 => called::<2>(site, leaves)?,
        3 => called::<3>(site, leaves)?,
        4 => called::<4>(site, leaves)?,
        5 => called::<5>(site, leaves)?,
        6 => called::<6>(site, leaves)?,
        7 => called::<7>(site, leaves)?,
        _ => called::<IN_PLACE>(site, leaves)?,
    })
}

/// [`operator`]'s call of `N` operands, their values held in place.
fn called<const N: usize>(site: Site, leaves: Vec<Leaf>) -> Option<Node> {
    let leaves: [Leaf; N] = leaves.try_into().ok()?;
    Some(Box::new(move |slots: &[SlotState], cx: &mut Cx<'_>| {
        let mut values = [Value::Absent; N];
        for (value, leaf) in values.iter_mut().zip(&leaves) {
            *value = leaf.eval(slots, cx)?;
        }
        site.call(&values, cx)
    }))
}
