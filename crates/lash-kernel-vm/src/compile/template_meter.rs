//! Speculative scalar-path charges, measured with the ordinary formula owner.
use super::{Entry, Executable, Source};
use lash_kernel_doc::Value;
use lash_kernel_doc::plan::{FormCharge, TemplateMeter};
use lash_kernel_value::FunctionId;

#[derive(Default)]
pub(crate) struct MeteredCharges {
    pub(crate) events: Vec<(u64, Option<FunctionId>)>,
    pub(crate) pace: u64,
}

struct Collector<'a> {
    exe: &'a Executable,
    exact: bool,
    events: Vec<(u64, Option<FunctionId>, bool)>,
    valid: bool,
}

impl TemplateMeter for Collector<'_> {
    fn mark(&self) -> usize {
        self.events.len()
    }
    fn rewind(&mut self, mark: usize) {
        self.events.truncate(mark);
    }
    fn form(&mut self, function: FunctionId, form: FormCharge) {
        let (units, pace) = form.measurement();
        self.events.push((units, Some(function), pace));
    }
    fn bill(&mut self, function: FunctionId, args: &[Value], result: Value) {
        if !self.exact {
            return;
        }
        let Some(id) = self.exe.library_lib(&function) else {
            self.valid = false;
            return;
        };
        let lib = self.exe.lib(id);
        let units = lib.charge.evaluate(|source, measure| {
            let value = match source {
                Source::Arg(index) => args.get(index),
                Source::Result => Some(&result),
                Source::Nothing => None,
            };
            value.map_or(0, |value| {
                crate::data::immediate_measure(value, measure).unwrap_or_else(|| {
                    self.valid = false;
                    0
                })
            })
        });
        self.events.push((units, Some(function), false));
    }
}

impl Entry {
    pub(crate) fn metered(
        &self,
        args: &[Value],
        exe: &Executable,
        exact: bool,
    ) -> Option<(Value, MeteredCharges)> {
        let Self::Template {
            template, natives, ..
        } = self
        else {
            return None;
        };
        let entry = template.metered?;
        let mut collector = Collector {
            exe,
            exact,
            events: Vec::new(),
            valid: true,
        };
        let value = entry(args, natives, &mut collector)?;
        if !collector.valid {
            return None;
        }
        let pace = collector
            .events
            .iter()
            .filter(|(_, _, paced)| *paced)
            .map(|(units, _, _)| *units)
            .sum();
        let events = if exact {
            collector
                .events
                .into_iter()
                .map(|(units, function, _)| (units, function))
                .collect()
        } else {
            Vec::new()
        };
        Some((value, MeteredCharges { events, pace }))
    }
}
