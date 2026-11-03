//! A library body's final completion owes its caller's canonical formula.
use super::*;
use lash_kernel_state::arena::Arena;

#[derive(Clone)]
pub(crate) struct ReturnDebt {
    source: Arc<Source>,
    at: Point,
    units: u64,
}
impl ReturnDebt {
    pub(crate) fn bind(
        exe: &Executable,
        source: &Arc<Source>,
        at: Point,
        lib: LibId,
        arguments: &[Value],
        result: Value,
        arena: &Arena,
    ) -> Option<Self> {
        let function = exe.lib(lib);
        let code = match function.run {
            compile::LibRun::Body(code)
            | compile::LibRun::Native {
                body: Some(code), ..
            } => code,
            _ => return None,
        };
        if !Arc::ptr_eq(source, &exe.code(code).source_room)
            || source.owner
                != doc::constant_pool::PoolOwner::Function(function.specialization.function)
            || arguments.len() != function.arity
            || source
                .departure_exit(&at)
                .ok()?
                .interval()
                .resume()
                .is_some()
        {
            return None;
        }
        let mut failed = false;
        let units = function.charge.evaluate(|input, measure| {
            let value = match input {
                compile::Source::Arg(at) => arguments.get(at),
                compile::Source::Result => Some(&result),
                compile::Source::Nothing => None,
            };
            let Some(value) = value else {
                return 0;
            };
            let measured = if let Some(value) = crate::data::immediate_measure(value, measure) {
                Some(value)
            } else {
                match measure {
                    doc::Measure::Size => crate::data::view_size(arena, value).ok(),
                    doc::Measure::DeepSize => crate::data::view_deep_size(arena, value).ok(),
                    doc::Measure::NestedSize => crate::data::view_nested_size(arena, value).ok(),
                    doc::Measure::Magnitude => Some(crate::data::view_magnitude(arena, value)),
                }
            };
            measured.unwrap_or_else(|| {
                failed = true;
                0
            })
        });
        (!failed).then(|| Self {
            source: Arc::clone(source),
            at,
            units,
        })
    }
    pub(crate) fn matches(&self, source: &Arc<Source>, at: &Point, units: u64) -> bool {
        Arc::ptr_eq(&self.source, source) && self.at == *at && self.units == units
    }
}
