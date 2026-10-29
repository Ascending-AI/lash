//! Speculative source charges with optional recording and fallible measurement.
use super::{Entry, Executable, Source};
use lash_kernel_doc::Value;
use lash_kernel_doc::plan::{ChargeEvent, ChargeMark, FormCharge, TemplateMeter};
use lash_kernel_value::FunctionId;

pub(crate) struct SpeculativeCharges<E, M = Measured> {
    pub(crate) events: Vec<E>,
    pub(crate) measurement: M,
}

pub(crate) struct Measured {
    pub(crate) units: u64,
    pub(crate) pace: u64,
    valid: bool,
    record: bool,
}

pub(crate) type MeteredCharges = SpeculativeCharges<ChargeEvent>;

pub(crate) trait Measurement {
    fn mark(&self) -> ChargeMark;
    fn rewind(&mut self, mark: ChargeMark);
    fn append(&mut self, other: &mut Self);
    fn recording(&self) -> bool;
}

impl Measurement for Measured {
    fn mark(&self) -> ChargeMark {
        ChargeMark {
            units: self.units,
            pace: self.pace,
            valid: self.valid,
            events: 0,
        }
    }
    fn rewind(&mut self, mark: ChargeMark) {
        self.units = mark.units;
        self.pace = mark.pace;
        self.valid &= mark.valid;
    }
    fn append(&mut self, other: &mut Self) {
        self.units = self.units.saturating_add(other.units);
        self.pace = self.pace.saturating_add(other.pace);
        self.valid &= other.valid;
        other.units = 0;
        other.pace = 0;
        other.valid = true;
    }
    fn recording(&self) -> bool {
        self.record
    }
}

impl<E, M: Measurement> SpeculativeCharges<E, M> {
    pub(crate) fn mark(&self) -> ChargeMark {
        ChargeMark {
            events: self.events.len(),
            ..self.measurement.mark()
        }
    }
    pub(crate) fn rewind(&mut self, mark: ChargeMark) {
        self.measurement.rewind(mark);
        self.events.truncate(mark.events);
    }
    pub(crate) fn append(&mut self, other: &mut Self) {
        self.measurement.append(&mut other.measurement);
        if self.measurement.recording() {
            self.events.append(&mut other.events);
        }
    }
}

impl SpeculativeCharges<ChargeEvent> {
    pub(crate) fn new(record: bool) -> Self {
        Self {
            events: Vec::new(),
            measurement: Measured {
                units: 0,
                pace: 0,
                valid: true,
                record,
            },
        }
    }
    pub(crate) fn valid(&self) -> bool {
        self.measurement.valid
    }
    pub(crate) fn push(&mut self, units: u64, function: Option<FunctionId>, pace: bool) {
        self.measurement.units = self.measurement.units.saturating_add(units);
        if pace {
            self.measurement.pace = self.measurement.pace.saturating_add(units);
        }
        if self.measurement.record {
            self.events.push(ChargeEvent { units, function });
        }
    }
    pub(crate) fn form(&mut self, function: FunctionId, form: FormCharge, exact: bool) {
        let (units, pace) = form.measurement();
        if exact || pace {
            self.push(units, Some(function), pace);
        }
    }
    pub(crate) fn bill(
        &mut self,
        function: FunctionId,
        exact: bool,
        measure: impl FnOnce() -> Option<u64>,
    ) {
        if !exact {
            return;
        }
        match measure() {
            Some(units) => self.push(units, Some(function), false),
            None => self.measurement.valid = false,
        }
    }
}

struct Collector<'a> {
    exe: &'a Executable,
    exact: bool,
    charges: MeteredCharges,
}

impl TemplateMeter for Collector<'_> {
    fn mark(&self) -> ChargeMark {
        self.charges.mark()
    }
    fn rewind(&mut self, mark: ChargeMark) {
        self.charges.rewind(mark);
    }
    fn form(&mut self, function: FunctionId, form: FormCharge) {
        self.charges.form(function, form, self.exact);
    }
    fn bill(&mut self, function: FunctionId, args: &[Value], result: Value) {
        self.charges.bill(function, self.exact, || {
            let lib = self.exe.lib(self.exe.library_lib(&function)?);
            let mut valid = true;
            let units = lib.charge.evaluate(|source, measure| {
                let value = match source {
                    Source::Arg(index) => args.get(index),
                    Source::Result => Some(&result),
                    Source::Nothing => None,
                };
                value.map_or(0, |value| {
                    crate::data::immediate_measure(value, measure).unwrap_or_else(|| {
                        valid = false;
                        0
                    })
                })
            });
            valid.then_some(units)
        });
    }
}

impl Entry {
    pub(crate) fn metered(
        &self,
        args: &[Value],
        exe: &Executable,
        exact: bool,
        record: bool,
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
            charges: MeteredCharges::new(record && exact),
        };
        let value = entry(args, natives, &mut collector)?;
        collector
            .charges
            .valid()
            .then_some((value, collector.charges))
    }
}
