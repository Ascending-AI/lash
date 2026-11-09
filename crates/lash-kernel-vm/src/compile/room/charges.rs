//! Canonical charge closes compiled into a reduced prefix and a cold tape.
//! Quiet binding evaluates only changing formulas, never the fixed event list.
pub(crate) mod region_isa;
use super::{Point, Refused, Source};
use crate::compile::{self, Executable, LibId, expr};
use lash_kernel_doc::{self as doc, Value};
use lash_kernel_exec::{Op, OpRange, Weight};
use lash_kernel_value::{FunctionId, sites::Site};
use std::sync::Arc;
mod loop_header;
mod native_completion;
mod returning;
pub(in crate::compile::room) mod settled;
pub(crate) use returning::ReturnDebt;
#[derive(Clone, Copy)]
enum Coverage {
    LoopHeader(bool),
    NativeCompletion,
}

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
    formals: usize,
    plan: compile::Plan,
}

/// Fixed sums and formula multiplicities are the hot product. Ordered closes
/// remain cold for verification, exact suffix selection and detailed tape.
pub(crate) struct ChargeProgram {
    source: Arc<Source>,
    coverage: Coverage,
    native_depth: Option<u64>,
    statement: compile::StmtId,
    range: OpRange,
    closes: Box<[Close]>,
    formulas: Box<[Formula]>,
    fixed: u64,
    recording: bool,
    first: Point,
    through: Point,
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
    pub(crate) fn matches(&self, source: &Arc<Source>, first: &Point, through: &Point) -> bool {
        Arc::ptr_eq(source, &self.program.source)
            && &self.program.first == first
            && &self.program.through == through
    }
    /// The completion compiler authenticates operation depth and unpaid fees.
    pub(crate) fn native_allowance(&self, safe_point: u64) -> Option<crate::admission::Allowance> {
        let strict = match self.program.coverage {
            Coverage::LoopHeader(_) => return None,
            Coverage::NativeCompletion => false,
        };
        Some(crate::admission::Allowance {
            occurrences: Vec::new(),
            before: 0,
            units: self.units,
            close: None,
            strict,
            depth: self.program.native_depth?,
            safe_point,
        })
    }
    pub(crate) fn units(&self) -> u64 {
        self.units
    }
}

impl ChargeProgram {
    /// Re-derive closes from the canonical stream, independently of reduced
    /// sums. Equal totals cannot authenticate a reordered or omitted close.
    pub(crate) fn verify(&self, exe: &Executable, code: compile::CodeId) -> Result<(), Refused> {
        if !Arc::ptr_eq(&self.source, &exe.code(code).source_room)
            || self.recording != exe.charging()
        {
            return Err(Refused::Source);
        }
        let (closes, formulas, first) = match self.coverage {
            Coverage::LoopHeader(ending) => loop_header::derive(exe, code, self.statement, ending)?,
            Coverage::NativeCompletion => {
                native_completion::derive(exe, code, self.statement, self.range)?
            }
        };
        let native_depth = match self.coverage {
            Coverage::LoopHeader(_) => None,
            Coverage::NativeCompletion => Some(native_completion::depth(
                exe,
                self.statement,
                self.range.start,
            )?),
        };
        if self.native_depth != native_depth {
            return Err(Refused::Source);
        }
        if first != self.first
            || closes.last().map(|close| &close.at) != Some(&self.through)
            || closes != *self.closes
            || formulas.len() != self.formulas.len()
            || formulas
                .iter()
                .zip(&self.formulas)
                .any(|(expected, actual)| {
                    expected.at != actual.at
                        || expected.function != actual.function
                        || expected.arguments != actual.arguments
                        || expected.formals != actual.formals
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
        self.bind_measured(values, |_, _, _| None)
    }

    /// Read only the measurements demanded by the canonical formula. Prepared
    /// storage supplies measurements of detached results and resident inputs,
    /// never a fee total. Immediate facts always come from the actual values.
    pub(crate) fn bind_measured(
        &self,
        values: &[FormulaValues<'_>],
        mut measure: impl FnMut(usize, compile::Source, doc::Measure) -> Option<u64>,
    ) -> Option<ChargeDebt<'_>> {
        if values.len() != self.formulas.len() {
            return None;
        }
        let mut units = self.fixed;
        let mut measured = Vec::with_capacity(values.len());
        for (index, (formula, values)) in self.formulas.iter().zip(values).enumerate() {
            if values.arguments.len() != formula.arguments {
                return None;
            }
            let mut complete = true;
            let fee = formula.plan.evaluate(|source, kind| {
                let value = match source {
                    compile::Source::Arg(at) if at < formula.formals => {
                        Some(values.arguments.get(at).unwrap_or(&Value::Absent))
                    }
                    compile::Source::Arg(_) => None,
                    compile::Source::Result => Some(&values.result),
                    compile::Source::Nothing => None,
                };
                let Some(value) = value else {
                    return 0;
                };
                let amount = crate::data::immediate_measure(value, kind)
                    .or_else(|| measure(index, source, kind));
                match amount {
                    Some(amount) => amount,
                    None => {
                        complete = false;
                        0
                    }
                }
            });
            if !complete {
                return None;
            }
            units = units.checked_add(fee)?;
            measured.push(fee);
        }
        Some(ChargeDebt {
            program: self,
            formulas: measured,
            units,
        })
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

fn source_point(stream: &compile::Stream, site: &Site, op: Op) -> Point {
    // A dynamic callable is read before its arguments. Its local read shares
    // the action's site, but belongs to Open, never the call's Commit.
    if matches!(op, Op::Local { .. } | Op::Session { .. })
        && stream.nested.iter().any(|entry| {
            entry.site == *site
                && matches!(
                    entry.kind,
                    compile::NestedKind::Call(compile::NestedCallee::Read)
                        | compile::NestedKind::Spawn(compile::NestedCallee::Read)
                )
        })
    {
        return Point {
            site: site.clone(),
            phase: super::Phase::Open,
        };
    }
    if matches!(op, Op::Act) {
        return Point {
            site: site.clone(),
            phase: super::Phase::Enter,
        };
    }
    if let Op::Nested { act, .. } = op
        && stream
            .nested
            .get(act.ix())
            .is_some_and(|entry| matches!(entry.kind, compile::NestedKind::Call(_)))
    {
        return Point {
            site: site.clone(),
            phase: super::Phase::Enter,
        };
    }
    point(site, op)
}

/// A prefix may begin only at the original unpaid charge fence and end at
/// a complete close. It cannot enter midway through an unpaid region.
fn canonical_span(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
) -> Result<OpRange, Refused> {
    canonical_span_target(exe, code, statement, range, None)
}

fn canonical_span_target(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
    target: Option<compile::CodeId>,
) -> Result<OpRange, Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    let body = exe.code(code);
    let canonical = match stmt.form {
        compile::Form::While { test, .. }
            if range.start >= test.start && range.end() <= test.end() =>
        {
            test
        }
        _ => stmt.ops,
    };
    let closes = |pc: u32| {
        stream
            .ops
            .get(pc as usize)
            .is_some_and(|op| !op.kind().in_region() && !matches!(op, Op::Region { .. }))
    };
    if exe.stmt_at(code, &stmt.site) != Some(statement)
        || range.len == 0
        || range.start.checked_add(range.len).is_none()
        || range.start < canonical.start
        || range.end() > canonical.end()
        || (range.start != canonical.start && !closes(range.start - 1))
        || !closes(range.end() - 1)
        || !(if matches!(stmt.form, compile::Form::Action(_)) {
            // The action writes register zero through the semantic ABI, not
            // Op::writes. Verify its actual source call and finite rooted
            // continuation, instead of inventing a nested-op location.
            (super::region::calls::target::resolve(exe, code, statement, stmt.ops.start, target)
                .is_ok()
                || native_completion::canonical_root_native(exe, code, statement, target))
                && matches!(
                    stream
                        .ops
                        .get(stmt.ops.start as usize..stmt.ops.end() as usize),
                    Some([
                        Op::Act,
                        Op::Bind { .. }
                            | Op::WriteLocal { .. }
                            | Op::BindSession { .. }
                            | Op::WriteSession { .. }
                            | Op::End
                            | Op::Return { .. }
                    ])
                )
                && compile::expr::statement(stream, stmt.ops, true).is_some()
                && stream
                    .source_sites
                    .get((stmt.ops.end() - 1) as usize)
                    .and_then(Option::as_deref)
                    == Some(&stmt.site)
        } else {
            stmt.verify_holding_sites(stream, &body.source_room, canonical)
        })
    {
        return Err(Refused::Source);
    }
    Ok(canonical)
}

fn completion(
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
    if !matches!(lib.specialization.resource, super::CallSource::Native(f) | super::CallSource::Body(f) if f == function)
        || !lib.body_arity(arguments)
    {
        return Err(Refused::Source);
    }
    if charging {
        let Some(formula) = lib.header.metering.formula() else {
            return Ok(());
        };
        tape.push(TapeEvent::Formula(formulas.len()));
        formulas.push(Formula {
            at,
            function,
            arguments,
            formals: lib.arity,
            plan: compile::Plan::new(formula, &lib.header.signature.params),
        });
    }
    Ok(())
}

pub(crate) fn verify_close(
    close: Option<&crate::compile::expr::Close>,
    stream: &crate::compile::Stream,
) -> Result<(), Refused> {
    let Some(close) = close else {
        return Ok(());
    };
    let refused = Refused::Exit;
    if !matches!(
        stream.ops.get(close.head as usize),
        Some(Op::Region { region, total }) if region.0 == close.region && *total == close.total
    ) {
        return Err(refused);
    }
    let stated = stream
        .regions
        .summary(close.region)
        .map_err(|_| Refused::Exit)?;
    if stated.total() == u64::from(close.total) {
        Ok(())
    } else {
        Err(refused)
    }
}
