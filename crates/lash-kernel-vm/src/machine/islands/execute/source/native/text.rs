//! A native's detached text and canonical page recipe share one registration.
use super::*;
use lash_kernel_state::arena::PreparedBatch;
use publication::{PreparedStorage, StoragePreparation, native_fault};

pub(super) struct TextNative<'a> {
    pub(super) function: &'a crate::compile::Lib,
    pub(super) args: Vec<Value>,
    pub(super) output: Owner,
    pub(super) charging: bool,
    pub(super) limits: lash_kernel_value::data_limits::KernelDataLimits,
    pub(super) room: Option<u64>,
}
pub(super) struct PreparedText {
    pub(super) batch: PreparedBatch,
    pub(super) value: Value,
    units: u64,
}

impl StoragePreparation for TextNative<'_> {
    type Prepared = PreparedText;

    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, PreparedText>>, HeapFault> {
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
        let LibRun::Native { native, .. } = &self.function.run else {
            return Ok(None);
        };
        if self.function.limit.is_some() {
            return Ok(None);
        }
        let semantics = arena.semantics().with_limits(self.limits);
        let mut nursery = match self.room {
            Some(room) => doc::Nursery::new(arena.objects(), doc::heap::LogicalBytes(room)),
            None => doc::Nursery::unaccounted(arena.objects()),
        };
        let mut counter = doc::WorkCounter::new(None);
        let answer = native.call(doc::NativeCall {
            args: doc::Args::new(&self.args, arena.objects(), &semantics),
            new: &mut nursery,
            counter: &mut counter,
        });
        let value = match answer {
            Ok(value) => value,
            Err(error) => {
                nursery.discard();
                return match error {
                    doc::NativeError::Host(error) => Err(error),
                    doc::NativeError::Malformed(reason) => Err(malformed(reason)),
                    _ => Ok(None),
                };
            }
        };
        // These guards retain the old physical eligibility until the complete
        // authority cutover. They never supply the logical native obligation.
        if self.room.is_some_and(|room| {
            arena
                .install_bound(nursery.built())
                .checked_add(nursery.held().0)
                .is_none_or(|bytes| bytes > room)
        }) {
            nursery.discard();
            return Ok(None);
        }
        let text: std::sync::Arc<str> = match value {
            Value::Text(short) if nursery.built().is_empty() => short.as_str().into(),
            Value::LongText(handle) => {
                let [(target, doc::NewObject::LongText(text))] = nursery.built() else {
                    nursery.discard();
                    return Ok(None);
                };
                if *target != handle {
                    nursery.discard();
                    return Ok(None);
                }
                std::sync::Arc::clone(text)
            }
            _ => {
                nursery.discard();
                return Ok(None);
            }
        };
        let units = if self.charging {
            let Some(units) = crate::machine::islands::cx::measured_text_formula(
                arena,
                &self.function.charge,
                &self.args,
                &text,
            ) else {
                nursery.discard();
                return Ok(None);
            };
            units
        } else {
            0
        };
        let page = doc::heap::PageValue::Text(doc::heap::TextRef {
            normal: Default::default(),
            summary: doc::heap_values::text_summary(&text),
            body: doc::heap::TextPayload::Resident(text),
        });
        let mut event = event.clone();
        event.outputs.push((self.output, PV::Page(page)));
        let actual = Program::new(vec![event]);
        let installation = arena
            .prepare_install(nursery.into_new())
            .map_err(native_fault)?;
        let (registration, batch) = installation.into_parts();
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedText {
                batch,
                value,
                units,
            },
            actual,
        }))
    }

    fn allowance(storage: &PreparedText, allowance: &mut Allowance) {
        allowance.units = storage.units;
    }
}
