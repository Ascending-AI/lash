//! A constant return compiles its only acquisition and terminal transfer once.
use super::*;
use crate::compile::{CodeId, Executable, StmtId, expr::Close};
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, Weight};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

pub(super) struct LiteralReturn {
    value: Value,
    statement: StmtId,
    reg: u32,
    close: Option<Close>,
    units: u64,
    tape: Box<[Weight]>,
    charged: bool,
    recording: bool,
    recovery: [u8; 32],
}

impl Source {
    pub(crate) fn literal_return_exit(
        self: &Arc<Self>,
        exe: &Executable,
        code_id: CodeId,
    ) -> Result<Arc<DepartureExit>, Refused> {
        let code = exe.code(code_id);
        if !Arc::ptr_eq(self, &code.source_room) {
            return Err(Refused::Source);
        }
        let first = Point {
            site: self.site.child(0).child(0),
            phase: Phase::Commit,
        };
        if let Some(exit) = self
            .departures
            .lock()
            .map_err(|_| Refused::Source)?
            .get(&first)
        {
            return exit
                .literal
                .as_ref()
                .filter(|literal| {
                    literal.charged == code.charged && literal.recording == exe.charging()
                })
                .map(|_| Arc::clone(exit))
                .ok_or(Refused::Source);
        }
        let [
            doc::Stmt::Return {
                value: doc::Expr::Literal(authored),
            },
        ] = self.body.as_slice()
        else {
            return Err(Refused::Source);
        };
        let crate::compile::Constant::Resident(value) =
            crate::compile::Constant::from(authored.clone())
        else {
            return Err(Refused::Source);
        };
        if value.handle().is_some() {
            return Err(Refused::Source);
        }
        let [statement_id] = exe.block(code.body).stmts.as_slice() else {
            return Err(Refused::Source);
        };
        let (stream, statement, _) = exe.statement(*statement_id);
        if statement.site != self.site.child(0) {
            return Err(Refused::Source);
        }
        let mut close = None;
        let mut constant = None;
        let mut returned = None;
        for pc in statement.ops.start..statement.ops.end() {
            match stream.ops[pc as usize] {
                Op::Region { region, total } if close.is_none() && constant.is_none() => {
                    close = Some(Close {
                        region: region.0,
                        head: pc,
                        total,
                    });
                }
                Op::Const { dst, c } if constant.is_none() => {
                    let Some(crate::compile::Constant::Resident(actual)) =
                        stream.consts.get(c.ix())
                    else {
                        return Err(Refused::Source);
                    };
                    if !actual.same(&value) {
                        return Err(Refused::Source);
                    }
                    constant = Some(dst);
                }
                Op::Return { src } if pc + 1 == statement.ops.end() && constant == Some(src) => {
                    returned = Some(src)
                }
                _ => return Err(Refused::Source),
            }
        }
        let reg = returned.ok_or(Refused::Source)?.0;
        let charges = super::super::super::charges::ChargeProgram::compile(
            exe,
            code_id,
            *statement_id,
            statement.ops,
        )?;
        if charges.formulas().next().is_some() {
            return Err(Refused::Source);
        }
        let debt = charges.bind(&[]).ok_or(Refused::Facts)?;
        let tape = crate::observe::ChargeTape::new();
        debt.record(&tape);
        let mut recovery = TokenWriter::new();
        recovery.seq(3);
        recovery.text("lash.source.compiled-literal-return");
        self.site.encode(&mut recovery);
        first.site.encode(&mut recovery);
        let literal = LiteralReturn {
            value,
            statement: *statement_id,
            reg,
            close,
            units: debt.units(),
            tape: tape.steps().into(),
            charged: code.charged,
            recording: exe.charging(),
            recovery: logical_digest(&recovery.into_bytes()),
        };
        let returning = Point {
            site: statement.site.clone(),
            phase: Phase::Commit,
        };
        let last = Point {
            site: self.site.clone(),
            phase: Phase::Leave,
        };
        let route = [
            Selection::Event {
                at: Point {
                    site: self.site.clone(),
                    phase: Phase::Enter,
                },
                exit: ExitKind::Normal,
            },
            Selection::Event {
                at: first.clone(),
                exit: ExitKind::Normal,
            },
            Selection::Event {
                at: returning.clone(),
                exit: ExitKind::Return,
            },
            Selection::Event {
                at: last.clone(),
                exit: ExitKind::Normal,
            },
        ];
        let interval = self.compile_between(&first, &last, &route, route.len())?;
        let mut events = interval.events();
        let operand = Owner::Operand(first.site.clone());
        let active = Owner::ActiveDeparture(self.site.clone());
        let read = events.next().ok_or(Refused::Source)?;
        let transfer = events.next().ok_or(Refused::Source)?;
        let leave = events.next().ok_or(Refused::Source)?;
        if read.point != first
            || !read.inputs.is_empty()
            || read.output.as_ref() != Some(&operand)
            || !read.entities.is_empty()
            || read.peak != Amount::default().term(Measure::Local(operand.clone()), 1)
            || read.delta != read.peak
            || transfer.point != returning
            || transfer.inputs.as_ref() != [operand]
            || transfer.output.as_ref() != Some(&active)
            || !transfer.entities.is_empty()
            || transfer.peak != Amount::default()
            || transfer.delta != Amount::default()
            || leave.point != last
            || !leave.inputs.is_empty()
            || leave.output.is_some()
            || !leave.entities.is_empty()
            || leave.peak != Amount::default()
            || leave.delta
                != (Amount {
                    fixed: -i128::from(Cost::FRAME),
                    ..Amount::default().term(
                        Measure::Release(
                            vec![
                                Owner::Scope(self.site.clone()),
                                Owner::Operands(self.site.clone()),
                            ]
                            .into(),
                        ),
                        -1,
                    )
                })
            || events.next().is_some()
            || interval.exit() != ExitKind::Return
        {
            return Err(Refused::Source);
        }
        drop(events);
        let exit = Arc::new(DepartureExit {
            literal: Some(literal),
            interval,
            first: first.clone(),
            last,
            input: None,
            output: None,
            releases: Box::default(),
            terminal: true,
        });
        let mut cache = self.departures.lock().map_err(|_| Refused::Source)?;
        Ok(Arc::clone(cache.entry(first).or_insert(exit)))
    }
}

impl DepartureExit {
    pub(crate) fn literal_matches(
        &self,
        statement: StmtId,
        reg: u32,
        close: Option<Close>,
        value: Value,
    ) -> bool {
        self.literal.as_ref().is_some_and(|literal| {
            literal.statement == statement
                && literal.reg == reg
                && literal.close == close
                && literal.value.same(&value)
        })
    }
    pub(crate) fn literal_start(&self, occurrence: Occurrence) -> Position {
        Position {
            at: self.first.clone(),
            unpaid: Some(self.first.clone()),
            recovery: self
                .literal
                .as_ref()
                .map_or([0; 32], |literal| literal.recovery),
            occurrence,
        }
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated constant-return boundary"
    )]
    pub(crate) fn prepare_literal<'a, 'r>(
        &self,
        source: &Arc<Source>,
        frontier: &Frontier,
        invocation: &Invocation,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        value: Value,
        resources: SourceResources<'r>,
        memory_bound: u64,
    ) -> Result<Option<SourceAccepted<'r, LiteralPublication<'a>>>, HeapFault> {
        let Some(literal) = &self.literal else {
            return Ok(None);
        };
        let before = &frontier.position;
        if frontier.proven
            || !Arc::ptr_eq(source, &frontier.source)
            || before.at != self.first
            || before.unpaid.as_ref() != Some(&self.first)
            || before.recovery != literal.recovery
            || !before.occurrence.loops.is_empty()
            || before.occurrence.action.is_some()
            || before.occurrence.invocation != invocation.id()
            || invocation.frame()
                != (Root::Frame {
                    task: before.occurrence.task,
                    invocation: invocation.id(),
                })
            || !literal.value.same(&value)
            || value.handle().is_some()
            || !source
                .departures
                .lock()
                .map_err(|_| fault("literal cache unavailable"))?
                .get(&self.first)
                .is_some_and(|exit| std::ptr::eq(self, exit.as_ref()))
        {
            return Ok(None);
        }
        let output = Root::Incoming(before.occurrence.task);
        if ledger.holding(output, heap.pages())?.is_some() {
            return Ok(None);
        }
        let bytes =
            logical::Holding::from_native(registration.arena(), heap.pages(), &literal.value)?
                .bytes();
        let mut transaction = ledger
            .prepare_leave_publication(
                invocation,
                registration,
                heap.pages(),
                &[(output, RootValue::Value(&literal.value))],
            )?
            .require_source_room(bytes)
            .ok_or_else(|| fault("literal source peak overflow"))?;
        if transaction.peak() > memory_bound {
            transaction = transaction.collect(ledger, heap.pages())?;
        }
        if transaction.peak() > memory_bound {
            return Ok(None);
        }
        let need = Allowance {
            occurrences: vec![],
            before: 0,
            units: literal.units,
            close: None,
            strict: false,
            depth: 0,
            safe_point: resources.safe_point,
        };
        let Some(facts) = resources.facts(&need) else {
            return Ok(None);
        };
        resources.admit_interval(
            &facts,
            &need,
            LiteralPublication {
                frontier: std::rc::Rc::clone(&frontier.identity),
                revision: frontier.revision,
                before: before.clone(),
                next: Position {
                    at: self.last.clone(),
                    unpaid: None,
                    ..before.clone()
                },
                transaction,
                memory_bound,
            },
        )
    }
    pub(crate) fn record_literal(&self, tape: &crate::observe::ChargeTape) {
        if let Some(literal) = &self.literal {
            for weight in &literal.tape {
                tape.push(*weight);
            }
        }
    }
}

pub(crate) struct LiteralPublication<'a> {
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    transaction: BoundaryTransaction<'a>,
    memory_bound: u64,
}
impl SourceAccepted<'_, LiteralPublication<'_>> {
    pub(crate) fn publish_literal(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
    ) -> Result<logical::Balance, HeapFault> {
        self.commit(|mut prepared| {
            if !std::rc::Rc::ptr_eq(&prepared.frontier, &frontier.identity)
                || prepared.revision != frontier.revision
                || frontier.proven
                || prepared.before != frontier.position
                || !prepared.transaction.is_current(ledger)
            {
                return Err(fault("literal frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("literal frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            prepared
                .transaction
                .admit(prepared.memory_bound)
                .map_err(|_| fault("literal admission changed"))?
                .publish(ledger, |_, edits| assert!(edits.is_empty()))?;
            frontier.revision = revision;
            frontier.position = prepared.next;
            Ok(balance)
        })
    }
}
