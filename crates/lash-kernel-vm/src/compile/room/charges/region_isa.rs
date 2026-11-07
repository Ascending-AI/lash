//! Canonical charge positions retained independently of ISA operation lowering.
use super::*;
use crate::compile::{CodeId, Form, StmtId, region_isa::KirOrigin};
use lash_kernel_exec::ChargeEvent;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChargeMoment {
    Before,
    After,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChargeOrigin {
    Region { region: u32, event: u32, pc: u32 },
    Statement(StmtId),
    LoopTest(StmtId),
    Scheduled(StmtId),
    Call { statement: StmtId, pc: u32 },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChargeItem {
    Fixed {
        event: ChargeEvent,
        weight: Weight,
        origin: ChargeOrigin,
    },
    Formula {
        event: ChargeEvent,
        function: FunctionId,
        arguments: usize,
        formals: usize,
        origin: ChargeOrigin,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalClose {
    pub(crate) at: Point,
    pub(crate) moment: ChargeMoment,
    pub(crate) regions: Box<[expr::Close]>,
    pub(crate) tape: Box<[ChargeItem]>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoopInfo {
    pub(crate) statement: StmtId,
    pub(crate) site: Site,
    pub(crate) while_loop: bool,
    pub(crate) entry: Point,
    pub(crate) test: Point,
    pub(crate) enter_origin: KirOrigin,
    pub(crate) test_origin: KirOrigin,
    pub(crate) entry_charges: Box<[CanonicalClose]>,
    pub(crate) test_charges: Box<[CanonicalClose]>,
    /// A protocol `for`'s hooks besides `open` (`K-ITER-005`).
    pub(crate) via: Option<ViaHooks>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViaHooks {
    pub(crate) next: bool,
    pub(crate) check: bool,
    pub(crate) item: bool,
}
pub(crate) struct Program {
    pub(crate) events: BTreeMap<Point, Vec<CanonicalClose>>,
    pub(crate) loops: Box<[LoopInfo]>,
}
pub(crate) fn terms(amount: &super::super::Amount) -> &[(super::super::Measure, i128)] {
    &amount.terms
}
fn fixed(at: Point, event: ChargeEvent, weight: Weight, origin: ChargeOrigin) -> CanonicalClose {
    CanonicalClose {
        at,
        moment: ChargeMoment::Before,
        regions: Box::new([]),
        tape: Box::new([ChargeItem::Fixed {
            event,
            weight,
            origin,
        }]),
    }
}

/// The existing stream is independently source-checked before its event
/// positions are read. Fixed events precede the physical operation they price;
/// result formulas follow the actual operation. No native entry is called.
pub(crate) fn program(exe: &Executable, code: CodeId) -> Result<Program, Refused> {
    let definition = exe.code(code);
    let source = &definition.source_room;
    source.verify()?;
    let mut events: BTreeMap<Point, Vec<CanonicalClose>> = BTreeMap::new();
    let mut loops = Vec::new();
    let mut blocks = vec![definition.body];
    let mut seen_blocks = BTreeSet::new();
    let emit = |events: &mut BTreeMap<Point, Vec<CanonicalClose>>, close: CanonicalClose| {
        events.entry(close.at.clone()).or_default().push(close);
    };
    while let Some(block) = blocks.pop() {
        if !seen_blocks.insert(block) {
            continue;
        }
        for statement in &exe.block(block).stmts {
            let (stream, stmt, _) = exe.statement(*statement);
            blocks.extend(stmt.form.blocks());
            let mut ranges = vec![stmt.ops];
            if let Form::While { test, .. } = stmt.form {
                ranges.push(test);
            }
            for range in &ranges {
                if !stmt.verify_holding_sites(stream, source, *range) {
                    return Err(Refused::Source);
                }
            }
            let statement_at = source.first_event(&stmt.site)?;
            let statement_fee = || {
                fixed(
                    statement_at.clone(),
                    ChargeEvent::Statement,
                    Weight::Units(doc::plan::FormCharge::Statement.measurement().0),
                    ChargeOrigin::Statement(*statement),
                )
            };
            if definition.charged && !matches!(stmt.form, Form::While { .. }) {
                emit(&mut events, statement_fee());
            }
            match &stmt.form {
                Form::While { metered, .. } | Form::For { metered, .. } => {
                    let while_loop = matches!(stmt.form, Form::While { .. });
                    let enter_pc = (stmt.ops.start..stmt.ops.end())
                        .find(|pc| matches!(stream.ops[*pc as usize], Op::While | Op::ForIn { .. }))
                        .ok_or(Refused::Source)?;
                    let enter_origin = KirOrigin {
                        statement: *statement,
                        pc: enter_pc,
                        op: stream.ops[enter_pc as usize],
                    };
                    let (entry, test_at, test_pc) = if while_loop {
                        let Form::While { test, .. } = stmt.form else {
                            return Err(Refused::Source);
                        };
                        let at = source.first_event(&stmt.site.child(0))?;
                        (at.clone(), at, test.start)
                    } else {
                        let Form::For { ref via, .. } = stmt.form else {
                            return Err(Refused::Source);
                        };
                        let entry = Point {
                            site: stmt.site.clone(),
                            phase: if via.is_some() {
                                super::super::Phase::Open
                            } else {
                                super::super::Phase::Test
                            },
                        };
                        // A pass is charged before any of its hooks, at the
                        // test stage that dispatches it (K-ITER-005).
                        let test_at = Point {
                            site: stmt.site.clone(),
                            phase: super::super::Phase::Test,
                        };
                        (entry, test_at, enter_pc)
                    };
                    let mut test_charges = Vec::new();
                    if definition.charged {
                        let cost = crate::costs::Costs::of_document(&exe.document).loop_test;
                        test_charges.push(fixed(
                            test_at.clone(),
                            ChargeEvent::LoopTest,
                            Weight::Atomic(cost),
                            ChargeOrigin::LoopTest(*statement),
                        ));
                    }
                    if *metered != 0 {
                        test_charges.push(fixed(
                            test_at.clone(),
                            ChargeEvent::LoopTest,
                            Weight::Atomic(*metered),
                            ChargeOrigin::Scheduled(*statement),
                        ));
                    }
                    loops.push(LoopInfo {
                        statement: *statement,
                        site: stmt.site.clone(),
                        while_loop,
                        entry,
                        test: test_at,
                        enter_origin,
                        test_origin: KirOrigin {
                            statement: *statement,
                            pc: test_pc,
                            op: *stream.ops.get(test_pc as usize).ok_or(Refused::Source)?,
                        },
                        entry_charges: if while_loop && definition.charged {
                            Box::new([statement_fee()])
                        } else {
                            Box::new([])
                        },
                        test_charges: test_charges.into(),
                        via: match &stmt.form {
                            Form::For { via: Some(via), .. } => Some(ViaHooks {
                                next: via.next.is_some(),
                                check: via.check.is_some(),
                                item: via.item.is_some(),
                            }),
                            _ => None,
                        },
                    });
                }
                _ => {}
            }
            let pcs: BTreeSet<_> = ranges
                .iter()
                .flat_map(|range| range.start..range.end())
                .collect();
            let mut seen_regions = BTreeSet::new();
            for pc in pcs {
                let op = *stream.ops.get(pc as usize).ok_or(Refused::Path)?;
                if let Op::Region { region, total } = op {
                    if !seen_regions.insert(region.0) {
                        continue;
                    }
                    let close = expr::Close {
                        region: region.0,
                        head: pc,
                        total,
                    };
                    compile::islands::verify_close(Some(&close), stream)
                        .map_err(|_| Refused::Source)?;
                    if !definition.charged {
                        continue;
                    }
                    for (index, (event, position)) in stream
                        .regions
                        .events(region.0)
                        .iter()
                        .zip(stream.regions.at(region.0))
                        .enumerate()
                    {
                        let actual = *stream.ops.get(*position as usize).ok_or(Refused::Path)?;
                        let site = stream
                            .source_sites
                            .get(*position as usize)
                            .and_then(Option::as_deref)
                            .ok_or(Refused::Source)?;
                        emit(
                            &mut events,
                            CanonicalClose {
                                at: source_point(stream, site, actual),
                                moment: ChargeMoment::Before,
                                regions: Box::new([close]),
                                tape: Box::new([ChargeItem::Fixed {
                                    event: event.event,
                                    weight: event.weight,
                                    origin: ChargeOrigin::Region {
                                        region: region.0,
                                        event: u32::try_from(index).map_err(|_| Refused::Facts)?,
                                        pc: *position,
                                    },
                                }]),
                            },
                        );
                    }
                }
                // Statement actions read their atom arguments outside expression
                // regions. Their Enter owns those unit charges, just as `atom`
                // does in the source executor (K-CHG-002).
                if op == Op::Act && definition.charged {
                    let Form::Action(action) = &stmt.form else {
                        return Err(Refused::Source);
                    };
                    let arguments = match &action.kind {
                        compile::ActionKind::Call { args, .. }
                        | compile::ActionKind::Perform { args, .. }
                        | compile::ActionKind::Spawn { args, .. } => args.len(),
                        compile::ActionKind::Join(_)
                        | compile::ActionKind::JoinMany(_, _)
                        | compile::ActionKind::Cancel(_)
                        | compile::ActionKind::CancelEffect(_) => 1,
                        compile::ActionKind::Yield => 0,
                    };
                    if arguments != 0 {
                        emit(
                            &mut events,
                            fixed(
                                Point {
                                    site: action.site.clone(),
                                    phase: super::super::Phase::Enter,
                                },
                                ChargeEvent::AtomArg,
                                Weight::Units(arguments as u64),
                                ChargeOrigin::Call {
                                    statement: *statement,
                                    pc,
                                },
                            ),
                        );
                    }
                }
                if !definition.charged {
                    continue;
                }
                let call = match op {
                    Op::Call { lib, args } | Op::Operator { lib, args } => {
                        Some((compile::LibId(lib.0), args.len as usize))
                    }
                    Op::Act => match &stmt.form {
                        Form::Action(compile::Action {
                            kind: compile::ActionKind::Call { callee, args },
                            ..
                        }) => callee.library().map(|lib| (lib, args.len())),
                        _ => None,
                    },
                    Op::Nested { act, args } => stream
                        .nested
                        .get(act.ix())
                        .and_then(|nested| nested.kind.operator_lib())
                        .map(|lib| (lib, args.len as usize)),
                    _ => None,
                };
                if let Some((lib, arguments)) = call {
                    let function = exe.lib(lib);
                    let site = stream
                        .source_sites
                        .get(pc as usize)
                        .and_then(Option::as_deref)
                        .ok_or(Refused::Source)?;
                    let at = source_point(stream, site, op);
                    let event = match function.specialization.resource {
                        super::super::CallSource::Native(_) => ChargeEvent::NativeFormula,
                        super::super::CallSource::Body(_) => ChargeEvent::BodyFormula,
                        _ => return Err(Refused::Source),
                    };
                    emit(
                        &mut events,
                        CanonicalClose {
                            at,
                            moment: ChargeMoment::After,
                            regions: Box::new([]),
                            tape: Box::new([ChargeItem::Formula {
                                event,
                                function: function.specialization.function,
                                arguments,
                                formals: function.arity,
                                origin: ChargeOrigin::Call {
                                    statement: *statement,
                                    pc,
                                },
                            }]),
                        },
                    );
                }
            }
        }
    }
    loops.sort_by_key(|loop_| loop_.statement);
    Ok(Program {
        events,
        loops: loops.into(),
    })
}
