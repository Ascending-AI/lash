//! A native's scalar or borrowed result needs no guest body installation.
use super::*;
use lash_kernel_state::kernel_internals::logical::{Holding, Ledger, RootValue};

pub(in crate::machine) struct ValueNative<'a> {
    pub(in crate::machine) function: &'a crate::compile::Lib,
    pub(in crate::machine) args: Vec<Value>,
    pub(in crate::machine) output: Owner,
    pub(in crate::machine) at: Point,
    pub(in crate::machine) release: Box<[room::Owner]>,
    pub(in crate::machine) ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(in crate::machine) heap: &'a PagedHeap,
    pub(in crate::machine) charging: bool,
    pub(in crate::machine) limits: lash_kernel_value::data_limits::KernelDataLimits,
    /// Only the checked prepared entry's actual scalar answer is supplied.
    pub(in crate::machine) known: Option<(Value, u32)>,
}
pub(in crate::machine) struct PreparedValue {
    pub(in crate::machine) value: Value,
    pub(in crate::machine) units: u64,
    pub(in crate::machine) work: u64,
    pub(in crate::machine) known: bool,
    depth: u32,
    at: Point,
    release: Box<[room::Owner]>,
    released: u64,
}
impl StoragePreparation for ValueNative<'_> {
    type Prepared = PreparedValue;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.outputs.is_empty()
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
        {
            return Ok(None);
        }
        let (value, work, depth) = if let Some((value, depth)) = self.known {
            (value, 0, depth)
        } else {
            let LibRun::Native { native, .. } = &self.function.run else {
                return Ok(None);
            };
            if self.function.limit.is_some() {
                return Ok(None);
            }
            let semantics = arena.semantics().with_limits(self.limits);
            let mut nursery = doc::Nursery::unaccounted(arena.objects());
            let mut counter = doc::WorkCounter::new(None);
            let answer = native.call(doc::NativeCall {
                args: doc::Args::new(&self.args, arena.objects(), &semantics),
                new: &mut nursery,
                counter: &mut counter,
            });
            let value = match answer {
                Ok(value) if nursery.built().is_empty() => value,
                Ok(_) => {
                    nursery.discard();
                    return Ok(None);
                }
                Err(error) => {
                    nursery.discard();
                    return match error {
                        doc::NativeError::Host(error) => Err(error),
                        doc::NativeError::Malformed(reason) => Err(malformed(reason)),
                        _ => Ok(None),
                    };
                }
            };
            nursery.discard();
            (value, counter.spent(), 0)
        };
        let units = if self.charging {
            let Some(units) = crate::machine::islands::cx::measured_formula(
                arena,
                &self.function.charge,
                &self.args,
                Some(&value),
            ) else {
                return Ok(None);
            };
            units
        } else {
            0
        };
        let mut registration = arena.registration().map_err(native_fault)?;
        // Projection owns every price and edge, including a child borrowed
        // from the sole input body that this event is about to release.
        Holding::from_native(registration.arena(), self.heap.pages(), &value)?;
        let released = self.ledger.borrow().release_cost_preserving(
            registration.arena(),
            self.heap,
            &event.inputs,
            &[(self.output, RootValue::Value(&value))],
        )?;
        let mut event = event.clone();
        event.outputs.push((self.output, PV::Native(value)));
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedValue {
                value,
                units,
                work,
                known: self.known.is_some(),
                depth,
                at: self.at,
                release: self.release,
                released,
            },
            actual: Program::new(vec![event]),
        }))
    }
    fn allowance(storage: &PreparedValue, allowance: &mut Allowance) {
        allowance.units = storage.units;
        allowance.depth = allowance.depth.max(u64::from(storage.depth));
    }
    fn facts(storage: &PreparedValue, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&storage.at)
            .ok_or_else(|| malformed("native value lost its release event"))?;
        facts.values.insert(
            Measure::Release(storage.release.clone()),
            i128::from(storage.released),
        );
        Ok(())
    }
}
