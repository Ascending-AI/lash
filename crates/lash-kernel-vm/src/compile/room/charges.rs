//! Canonical charge closes compiled into a reduced prefix and a cold tape.
//! Quiet binding evaluates only changing formulas, never the fixed event list.
use super::{Point, Refused, Source};
use crate::compile::{self, Executable, LibId, expr};
use lash_kernel_doc::{self as doc, Value};
use lash_kernel_exec::{Op, OpRange, Weight};
use lash_kernel_value::{FunctionId, sites::Site};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
enum TapeEvent {
    Fixed(Weight),
    Formula(usize),
}

/// A close keeps source identity in addition to its discardable code location.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Close {
    at: Point,
    regions: Box<[expr::Close]>,
    tape: Box<[TapeEvent]>,
}

struct Formula {
    at: Point,
    function: FunctionId,
    arguments: usize,
    plan: compile::Plan,
}

/// Fixed sums and formula multiplicities are the hot product. Ordered closes
/// remain cold for verification, exact suffix selection and detailed tape.
pub(crate) struct ChargeProgram {
    source: Arc<Source>,
    statement: compile::StmtId,
    range: OpRange,
    closes: Box<[Close]>,
    formulas: Box<[Formula]>,
    fixed: u64,
    recording: bool,
}

/// Actual evaluated operands, in source argument order, and the canonical
/// result. A caller cannot substitute a fee amount for these measurements.
pub(crate) struct FormulaValues<'a> {
    pub(crate) arguments: &'a [Value],
    pub(crate) result: Value,
}

/// A measured prefix is private to the charge compiler. Abandoning it has no
/// effect on the source meter, tape or occurrence counters.
pub(crate) struct ChargeDebt<'a> {
    program: &'a ChargeProgram,
    formulas: Vec<u64>,
    units: u64,
}

impl ChargeDebt<'_> {
    pub(crate) fn units(&self) -> u64 {
        self.units
    }

    pub(crate) fn record(&self, tape: &crate::observe::ChargeTape) {
        if !self.program.recording {
            return;
        }
        for close in &self.program.closes {
            for event in &close.tape {
                match *event {
                    TapeEvent::Fixed(weight) => tape.push(weight),
                    TapeEvent::Formula(index) => {
                        let units = self.formulas[index];
                        if units != 0 {
                            tape.push(Weight::Atomic(units));
                        }
                    }
                }
            }
        }
    }
}

impl ChargeProgram {
    /// Compile a straight-line statement or its canonical while-test range.
    /// Branch/call composition selects multiple such spans; it never invents
    /// a close or takes a fee total supplied by the packet emitter.
    pub(crate) fn compile(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        range: OpRange,
    ) -> Result<Self, Refused> {
        let source = Arc::clone(&exe.code(code).source_room);
        source.verify()?;
        let (stream, stmt, _) = exe.statement(statement);
        if exe.stmt_at(code, &stmt.site) != Some(statement)
            || !(range == stmt.ops
                || matches!(stmt.form, compile::Form::While { test, .. } if range == test))
            || !stream.verify_holding_sites(&source, range, stmt.regs)
        {
            return Err(Refused::Source);
        }
        let (closes, formulas) = derive(exe, code, statement, range)?;
        let fixed = closes
            .iter()
            .flat_map(|close| close.tape.iter())
            .try_fold(0_u64, |sum, event| match event {
                TapeEvent::Fixed(weight) => sum.checked_add(weight.units()),
                TapeEvent::Formula(_) => Some(sum),
            })
            .ok_or(Refused::Facts)?;
        let program = Self {
            source,
            statement,
            range,
            closes: closes.into(),
            formulas: formulas.into(),
            fixed,
            recording: exe.charging(),
        };
        program.verify(exe, code)?;
        Ok(program)
    }

    /// Re-derive closes from the canonical stream, independently of reduced
    /// sums. Equal totals cannot authenticate a reordered or omitted close.
    pub(crate) fn verify(&self, exe: &Executable, code: compile::CodeId) -> Result<(), Refused> {
        if !Arc::ptr_eq(&self.source, &exe.code(code).source_room)
            || self.recording != exe.charging()
        {
            return Err(Refused::Source);
        }
        let (closes, formulas) = derive(exe, code, self.statement, self.range)?;
        if closes != *self.closes
            || formulas.len() != self.formulas.len()
            || formulas
                .iter()
                .zip(&self.formulas)
                .any(|(expected, actual)| {
                    expected.at != actual.at
                        || expected.function != actual.function
                        || expected.arguments != actual.arguments
                })
        {
            return Err(Refused::Exit);
        }
        let fixed = closes
            .iter()
            .flat_map(|close| &close.tape)
            .try_fold(0_u64, |sum, event| match event {
                TapeEvent::Fixed(weight) => sum.checked_add(weight.units()),
                TapeEvent::Formula(_) => Some(sum),
            });
        if fixed != Some(self.fixed) {
            return Err(Refused::Facts);
        }
        Ok(())
    }

    pub(crate) fn formulas(&self) -> impl Iterator<Item = (&Point, FunctionId, usize)> {
        self.formulas
            .iter()
            .map(|formula| (&formula.at, formula.function, formula.arguments))
    }

    /// Exact canonical formulas retain their own saturating arithmetic. The
    /// sum across source charge events is checked, as the shared meter is.
    pub(crate) fn bind(&self, values: &[FormulaValues<'_>]) -> Option<ChargeDebt<'_>> {
        if values.len() != self.formulas.len() {
            return None;
        }
        let mut units = self.fixed;
        let mut measured = Vec::with_capacity(values.len());
        for (formula, values) in self.formulas.iter().zip(values) {
            if values.arguments.len() != formula.arguments {
                return None;
            }
            let fee = formula.plan.immediate(
                values.arguments,
                compile::Fresh::default(),
                &values.result,
            )?;
            units = units.checked_add(fee)?;
            measured.push(fee);
        }
        Some(ChargeDebt {
            program: self,
            formulas: measured,
            units,
        })
    }

    #[cfg(test)]
    pub(crate) fn corrupt_close_for_law(&mut self) {
        let close = self
            .closes
            .iter_mut()
            .find(|close| !close.regions.is_empty())
            .unwrap_or_else(|| unreachable!("law has a canonical close"));
        close.regions[0].total = close.regions[0].total.wrapping_add(1);
    }
}

fn point(site: &Site, op: Op) -> Point {
    use super::Phase;
    Point {
        site: site.clone(),
        phase: match op {
            Op::Branch { .. } | Op::Test { .. } | Op::JumpIfFalse { .. } => Phase::Test,
            _ => Phase::Commit,
        },
    }
}

/// This reads the original stream, not the optimizer's expression tree. Region
/// identity, head, event ordering and total are checked by the canonical table.
fn derive(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
) -> Result<(Vec<Close>, Vec<Formula>), Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    let code = exe.code(code);
    let mut closes = Vec::new();
    let mut formulas = Vec::new();
    let mut open = Vec::new();
    let mut tape = Vec::new();
    if range == stmt.ops && code.charged {
        tape.push(TapeEvent::Fixed(Weight::Units(
            doc::plan::FormCharge::Statement.measurement().0,
        )));
    }
    if matches!(stmt.form, compile::Form::While { test, .. } if range == test) {
        // The source's canonical loop-test event is outside its expression
        // region. Formula-metered helpers retain their declared loop meter.
        let kernel = match code.source_room.owner {
            doc::constant_pool::PoolOwner::Function(function) => {
                exe.lib(exe.library_lib(&function).ok_or(Refused::Source)?)
                    .header
                    .kernel
            }
            doc::constant_pool::PoolOwner::Document(_) => exe.document.kernel(),
        };
        let metered = match stmt.form {
            compile::Form::While { metered, .. } => metered,
            _ => return Err(Refused::Source),
        };
        let units = if code.charged {
            crate::costs::Costs::of(doc::KernelVersion::of(kernel).ok_or(Refused::Source)?)
                .loop_test
        } else {
            0
        }
        .checked_add(metered)
        .ok_or(Refused::Facts)?;
        if units != 0 {
            tape.push(TapeEvent::Fixed(Weight::Atomic(units)));
        }
    }
    for pc in range.start..range.end() {
        let op = *stream.ops.get(pc as usize).ok_or(Refused::Path)?;
        if let Op::Region { region, total } = op {
            let close = expr::Close {
                region: region.0,
                head: pc,
                total,
            };
            compile::islands::verify_close(Some(&close), stream).map_err(|_| Refused::Exit)?;
            open.push(close);
            tape.extend(
                stream
                    .regions
                    .events(region.0)
                    .iter()
                    .map(|event| TapeEvent::Fixed(event.weight)),
            );
            continue;
        }
        let site = stream
            .source_sites
            .get(pc as usize)
            .and_then(Option::as_deref)
            .ok_or(Refused::Source)?;
        if !op.kind().in_region() {
            match op {
                Op::Operator { lib, args } | Op::Call { lib, args } => {
                    native(
                        exe,
                        LibId(lib.0),
                        args.len as usize,
                        point(site, op),
                        &mut formulas,
                        &mut tape,
                        code.charging,
                    )?;
                }
                Op::Nested { act, args } => {
                    let nested = stream.nested.get(act.ix()).ok_or(Refused::Source)?;
                    let lib = nested.kind.operator_lib().ok_or(Refused::Source)?;
                    native(
                        exe,
                        lib,
                        args.len as usize,
                        point(site, op),
                        &mut formulas,
                        &mut tape,
                        code.charging,
                    )?;
                }
                Op::Bind { .. } | Op::WriteLocal { .. } | Op::End | Op::Test { .. } => {}
                // A source error/effect, guest call, dynamic branch, copy-out
                // or body formula needs its own authenticated composition.
                _ => return Err(Refused::Source),
            }
            closes.push(Close {
                at: point(site, op),
                regions: std::mem::take(&mut open).into(),
                tape: std::mem::take(&mut tape).into(),
            });
        }
    }
    if !open.is_empty() || !tape.is_empty() {
        return Err(Refused::Exit);
    }
    Ok((closes, formulas))
}

fn native(
    exe: &Executable,
    lib: LibId,
    arguments: usize,
    at: Point,
    formulas: &mut Vec<Formula>,
    tape: &mut Vec<TapeEvent>,
    charging: bool,
) -> Result<(), Refused> {
    let lib = exe.lib(lib);
    let function = lib.specialization.function;
    if lib.specialization.resource != super::CallSource::Native(function) {
        return Err(Refused::Source);
    }
    if charging {
        let formula = lib.header.metering.formula().ok_or(Refused::Source)?;
        tape.push(TapeEvent::Formula(formulas.len()));
        formulas.push(Formula {
            at,
            function,
            arguments,
            plan: compile::Plan::new(formula, &lib.header.signature.params),
        });
    }
    Ok(())
}

impl crate::admission::Window {
    /// A compiled charge prefix has already authenticated every close and
    /// measured its canonical formulas. Quiet admission uses its reduced sum.
    #[inline(always)]
    pub(crate) fn admit_charges<C>(
        self,
        request: C,
        debt: &crate::compile::room::charges::ChargeDebt<'_>,
    ) -> Option<crate::admission::Accepted<C>> {
        self.admit(request, 0, debt.units(), None, true)
    }
}

impl<'a> crate::admission::SourceResources<'a> {
    /// Compiled prefixes spend one authenticated charge debt together with
    /// their logical boundary. Resource staging is pure until commit.
    pub(crate) fn admit_region<C>(
        self,
        debt: &crate::compile::room::charges::ChargeDebt<'_>,
        occurrences: &[usize],
        depth: u64,
        safe_point: u64,
        request: C,
    ) -> Result<Option<crate::admission::SourceAccepted<'a, C>>, lash_kernel_doc::heap::HeapFault>
    {
        let need = crate::admission::Allowance {
            occurrences: occurrences.to_vec(),
            before: 0,
            units: debt.units(),
            close: None,
            strict: true,
            depth,
            safe_point,
        };
        let Some(facts) = self.facts(&need) else {
            return Ok(None);
        };
        self.admit_interval(&facts, &need, request)
    }
}
