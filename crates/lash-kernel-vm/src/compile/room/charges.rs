//! Canonical charge closes compiled into a reduced prefix and a cold tape.
//! Quiet binding evaluates only changing formulas, never the fixed event list.
use super::{Point, Refused, Source};
use crate::compile::{self, Executable, LibId, expr};
use lash_kernel_doc::{self as doc, Value};
use lash_kernel_exec::{Op, OpRange, Weight};
use lash_kernel_value::{FunctionId, sites::Site};
use std::sync::Arc;
pub(crate) mod composition;
mod event_packet;
pub(in crate::compile::room) mod settled;
pub(in crate::compile::room) use event_packet::prefix::OriginalPayment;
#[cfg(test)]
pub(crate) use event_packet::prefix::OriginalReplay;
pub(crate) use event_packet::prefix::{AcceptedOriginalPrefix, OriginalCompiler, OriginalPrefix};
mod loop_header;
mod native_completion;
mod returning;
pub(crate) use returning::ReturnDebt;
#[derive(Clone, Copy)]
enum Coverage {
    Span,
    LoopHeader(bool),
    Callback(compile::CodeId),
    NativeCompletion,
    NativePacket(Option<expr::Close>),
    NativeAfterCloses,
    EventPacket(Option<expr::Close>),
    ConstructorPacket { unpaid: u32, quote: u64 },
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
    native_occurrence: Option<usize>,
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
    /// The suffix compiler owns strictness, the original pending close and
    /// operation/template depth. Consumers cannot supply those independently.
    pub(crate) fn native_allowance(&self, safe_point: u64) -> Option<crate::admission::Allowance> {
        let strict = match self.program.coverage {
            Coverage::Span | Coverage::Callback(_) | Coverage::LoopHeader(_) => return None,
            Coverage::NativeCompletion | Coverage::ConstructorPacket { .. } => false,
            Coverage::NativePacket(_) | Coverage::NativeAfterCloses | Coverage::EventPacket(_) => {
                true
            }
        };
        Some(crate::admission::Allowance {
            occurrences: self.program.native_occurrence.into_iter().collect(),
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
        Self::compile_selected(exe, code, statement, range, None)
    }

    pub(in crate::compile::room) fn compile_callback(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        range: OpRange,
        target: compile::CodeId,
    ) -> Result<Self, Refused> {
        Self::compile_selected(exe, code, statement, range, Some(target))
    }

    fn compile_selected(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        range: OpRange,
        target: Option<compile::CodeId>,
    ) -> Result<Self, Refused> {
        let source = Arc::clone(&exe.code(code).source_room);
        source.verify()?;
        let (closes, formulas) = derive_target(exe, code, statement, range, target)?;
        let first = first_target(exe, code, statement, range, target)?;
        let through = closes.last().ok_or(Refused::Exit)?.at.clone();
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
            coverage: target.map_or(Coverage::Span, Coverage::Callback),
            native_depth: None,
            native_occurrence: None,
            statement,
            range,
            closes: closes.into(),
            formulas: formulas.into(),
            fixed,
            recording: exe.charging(),
            first,
            through,
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
        let (closes, formulas, first) = match self.coverage {
            Coverage::LoopHeader(ending) => loop_header::derive(exe, code, self.statement, ending)?,
            Coverage::Span => {
                let (closes, formulas) = derive(exe, code, self.statement, self.range)?;
                (
                    closes,
                    formulas,
                    first(exe, code, self.statement, self.range)?,
                )
            }
            Coverage::Callback(target) => {
                let (closes, formulas) =
                    derive_target(exe, code, self.statement, self.range, Some(target))?;
                (
                    closes,
                    formulas,
                    first_target(exe, code, self.statement, self.range, Some(target))?,
                )
            }
            Coverage::NativeCompletion => {
                native_completion::derive(exe, code, self.statement, self.range, None)?
            }
            Coverage::NativePacket(close) => {
                native_completion::derive(exe, code, self.statement, self.range, Some(close))?
            }
            Coverage::NativeAfterCloses => native_completion::after_closes(
                exe,
                code,
                self.statement,
                self.range,
                self.closes.first().ok_or(Refused::Source)?.regions.as_ref(),
            )?,
            Coverage::EventPacket(close) => {
                event_packet::derive(exe, code, self.statement, self.range, close)?
            }
            Coverage::ConstructorPacket { unpaid, quote } => event_packet::constructor::derive(
                exe,
                code,
                self.statement,
                self.range,
                unpaid,
                quote,
            )?,
        };
        let native_depth = match self.coverage {
            Coverage::Span | Coverage::Callback(_) | Coverage::LoopHeader(_) => None,
            Coverage::EventPacket(_) | Coverage::ConstructorPacket { .. } => Some(0),
            Coverage::NativeCompletion => Some(native_completion::depth(
                exe,
                self.statement,
                self.range.start,
                false,
            )?),
            Coverage::NativePacket(_) | Coverage::NativeAfterCloses => Some(
                native_completion::depth(exe, self.statement, self.range.start, true)?,
            ),
        };
        let native_occurrence = if matches!(self.coverage, Coverage::NativePacket(_)) {
            native_completion::root_occurrence(exe, self.statement, self.range.start)?
        } else {
            None
        };
        if self.native_occurrence != native_occurrence || self.native_depth != native_depth {
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

    pub(crate) fn first(&self) -> &Point {
        &self.first
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

fn source_point(stream: &compile::Stream, site: &Site, op: Op) -> Point {
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

fn first(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
) -> Result<Point, Refused> {
    first_target(exe, code, statement, range, None)
}

fn first_target(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
    target: Option<compile::CodeId>,
) -> Result<Point, Refused> {
    let canonical = canonical_span_target(exe, code, statement, range, target)?;
    let (stream, stmt, _) = exe.statement(statement);
    let source = &exe.code(code).source_room;
    if range.start == canonical.start {
        return source.first_event(&if canonical == stmt.ops {
            stmt.site.clone()
        } else {
            stmt.site.child(0)
        });
    }
    for pc in range.start..range.end() {
        let op = stream.ops[pc as usize];
        if matches!(op, Op::Region { .. } | Op::Pin { .. }) {
            continue;
        }
        let site = stream
            .source_sites
            .get(pc as usize)
            .and_then(Option::as_deref)
            .ok_or(Refused::Source)?;
        return Ok(source_point(stream, site, op));
    }
    Err(Refused::Path)
}

/// This reads the original stream, not the optimizer's expression tree. Region
/// identity, head, event ordering and total are checked by the canonical table.
fn derive(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
) -> Result<(Vec<Close>, Vec<Formula>), Refused> {
    derive_target(exe, code, statement, range, None)
}

fn derive_target(
    exe: &Executable,
    code: compile::CodeId,
    statement: compile::StmtId,
    range: OpRange,
    target: Option<compile::CodeId>,
) -> Result<(Vec<Close>, Vec<Formula>), Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    let canonical = canonical_span_target(exe, code, statement, range, target)?;
    let caller_code = code;
    let code = exe.code(code);
    let mut closes = Vec::new();
    let mut formulas = Vec::new();
    let mut open = Vec::new();
    let mut tape = Vec::new();
    if canonical == stmt.ops && range.start == stmt.ops.start && code.charged {
        tape.push(TapeEvent::Fixed(Weight::Units(
            doc::plan::FormCharge::Statement.measurement().0,
        )));
    }
    if matches!(stmt.form, compile::Form::While { test, .. } if canonical == test && range.start == test.start)
    {
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
            // Formula-metered bodies retain the original region identities,
            // but their caller's completion formula covers these forms. The
            // stream still contains expression events for cold recovery.
            if code.charged {
                tape.extend(
                    stream
                        .regions
                        .events(region.0)
                        .iter()
                        .map(|event| TapeEvent::Fixed(event.weight)),
                );
            }
            continue;
        }
        // Pin preserves the evaluated RHS while source target/index reads
        // run. canonical_span verified its role; it has no charge close.
        if matches!(op, Op::Pin { .. }) {
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
                    completion(
                        exe,
                        LibId(lib.0),
                        args.len as usize,
                        source_point(stream, site, op),
                        &mut formulas,
                        &mut tape,
                        code.charging,
                    )?;
                }
                Op::Act => {
                    let compile::Form::Action(action) = &stmt.form else {
                        return Err(Refused::Source);
                    };
                    let compile::ActionKind::Call { callee, args } = &action.kind else {
                        return Err(Refused::Source);
                    };
                    if pc != stmt.ops.start || action.site != *site {
                        return Err(Refused::Source);
                    }
                    if code.charged {
                        tape.extend(args.iter().map(|_| {
                            TapeEvent::Fixed(Weight::Atomic(
                                doc::plan::FormCharge::Argument.measurement().0,
                            ))
                        }));
                    }
                    match callee {
                        compile::Callee::Library(lib) | compile::Callee::CheckedHelper(lib) => {
                            completion(
                                exe,
                                *lib,
                                args.len(),
                                source_point(stream, site, op),
                                &mut formulas,
                                &mut tape,
                                code.charging,
                            )?
                        }
                        compile::Callee::Declared(_) => {}
                        compile::Callee::Value(_) => {
                            let resolved = super::region::calls::target::resolve(
                                exe,
                                caller_code,
                                statement,
                                pc,
                                target,
                            )?;
                            if let Some(lib) = resolved.library {
                                completion(
                                    exe,
                                    lib,
                                    args.len(),
                                    source_point(stream, site, op),
                                    &mut formulas,
                                    &mut tape,
                                    code.charging,
                                )?;
                            }
                        }
                        _ => return Err(Refused::Source),
                    }
                }
                Op::Nested { act, args } => {
                    let nested = stream.nested.get(act.ix()).ok_or(Refused::Source)?;
                    if let Some(lib) = nested.kind.operator_lib() {
                        completion(
                            exe,
                            lib,
                            args.len as usize,
                            source_point(stream, site, op),
                            &mut formulas,
                            &mut tape,
                            code.charging,
                        )?;
                    } else if matches!(
                        nested.kind,
                        compile::NestedKind::Call(compile::NestedCallee::Read)
                    ) {
                        let resolved = super::region::calls::target::resolve(
                            exe,
                            caller_code,
                            statement,
                            pc,
                            target,
                        )?;
                        if let Some(lib) = resolved.library {
                            completion(
                                exe,
                                lib,
                                args.len.checked_sub(1).ok_or(Refused::Source)? as usize,
                                source_point(stream, site, op),
                                &mut formulas,
                                &mut tape,
                                code.charging,
                            )?;
                        }
                    } else if !matches!(
                        nested.kind,
                        compile::NestedKind::Call(compile::NestedCallee::Declared(_))
                    ) {
                        return Err(Refused::Source);
                    }
                }
                Op::Bind { .. }
                | Op::WriteLocal { .. }
                | Op::SetIndex { .. }
                | Op::SetField { .. }
                | Op::Field { .. }
                | Op::Index { .. }
                | Op::End
                | Op::Test { .. }
                | Op::Branch { .. }
                | Op::Return { .. } => {}
                // A source error/effect, guest call, dynamic branch, copy-out
                // or body formula needs its own authenticated composition.
                _ => return Err(Refused::Source),
            }
            closes.push(Close {
                at: source_point(stream, site, op),
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
