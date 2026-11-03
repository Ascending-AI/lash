//! Local preparation proof. Full source/charge acceptance is core's gate.
use super::*;
#[path = "law/affine.rs"]
mod affine;
use crate::machine::islands::witness::Family;
use lash_kernel_doc::TaskId;

impl Compiled {
    /// Exercise the actual recognized sites. No test-only collection plan or
    /// synthesized source certificate substitutes for the compiler's result.
    pub(crate) fn check_collection_preparations(
        &self,
        exe: &Executable,
        heap: &PagedHeap,
        count: usize,
    ) -> Vec<Family> {
        let mut reached = Vec::new();
        let mut cases = Vec::new();
        cases.extend(
            self.code
                .sites
                .collection_append
                .iter()
                .map(|site| (CollectionCase::Append(&site.shape), false)),
        );
        cases.extend(
            self.code
                .sites
                .collection_reduce
                .iter()
                .map(|site| (CollectionCase::Reduce(&site.shape), false)),
        );
        cases.extend(
            self.code
                .sites
                .collection_placement
                .iter()
                .map(|site| (CollectionCase::Place(&site.shape), false)),
        );
        cases.extend(
            self.code
                .sites
                .collection_visit
                .iter()
                .map(|site| (CollectionCase::Visit(&site.shape), false)),
        );
        cases.extend(
            self.code
                .sites
                .collection_read
                .iter()
                .map(|site| (CollectionCase::ReadSlot(&site.shape), false)),
        );
        cases.extend(
            self.code
                .sites
                .collection_merge
                .iter()
                .map(|site| (CollectionCase::Merge(&site.shape), false)),
        );
        cases.extend(
            self.code
                .sites
                .collection_tail
                .iter()
                .map(|site| (CollectionCase::Merge(site.shape.shape()), true)),
        );
        for (step, tail) in &cases {
            let exits = CollectionExits::compile(&step.lowering(), exe, self)
                .expect("source exits compile with the finite site");
            for case in 0..3 {
                let mut arena = Arena::new();
                let hole = arena.tuple(vec![], 128).unwrap();
                let sorted = arena.create_list(&[hole, Value::Absent]);
                let out = arena.create_list(&[Value::Int(3), Value::Int(7)]);
                let mut slots = vec![SlotState::Value(Value::Int(0)); count];
                let mut set = |at, value| slots[at] = SlotState::Value(value);
                match step {
                    CollectionCase::Append(append) => {
                        set(append.target, out);
                        set(append.item, if case == 1 { out } else { sorted });
                    }
                    CollectionCase::Reduce(reduce) => {
                        set(reduce.accumulator, Value::Int(11));
                        set(reduce.item, Value::Int(7));
                    }
                    CollectionCase::Place(place) => {
                        set(place.target, out);
                        set(place.index, Value::Int(0));
                        set(place.value, sorted);
                        if let Some(list) = place.list {
                            set(list, Value::Bool(true));
                        }
                    }
                    CollectionCase::Visit(visit) => {
                        set(visit.index, Value::Int(0));
                        set(visit.length, Value::Int(2));
                        set(visit.value, if case == 0 { hole } else { Value::Absent });
                    }
                    CollectionCase::ReadSlot(read) => {
                        set(read.target, sorted);
                        set(read.index, Value::Int(i64::from(case != 0)));
                        set(read.list, Value::Bool(true));
                    }
                    CollectionCase::Merge(merge) => {
                        let counters = if *tail || case == 0 {
                            [0, 0, 0, 0, 0, 1, 2]
                        } else if case == 1 {
                            [0, 2, 1, 1, 1, 1, 2]
                        } else {
                            [0, 2, 1, 0, 2, 1, 2]
                        };
                        for (at, value) in merge.counters.iter().zip(counters) {
                            set(*at, Value::Int(value));
                        }
                        set(merge.lists[0], sorted);
                        set(merge.lists[1], out);
                        set(merge.order, Value::Int(if case == 0 { 1 } else { 0 }));
                        if *tail {
                            set(merge.from_right, Value::Bool(false));
                        }
                        // These are the saved pre-callback operands. Tail must
                        // read the current list, without overwriting either.
                        set(merge.items[0], Value::Int(41));
                        set(merge.items[1], Value::Int(42));
                    }
                }
                let mut ledger = Ledger::fresh();
                ledger
                    .prepare_source_boundary(
                        arena.registration().unwrap(),
                        heap,
                        &[],
                        &[
                            (Owner::Incoming(TaskId::MAIN), RootValue::Value(&out)),
                            (Owner::Ended(TaskId::MAIN), RootValue::Value(&sorted)),
                        ],
                    )
                    .unwrap()
                    .admit(u64::MAX)
                    .unwrap_or_else(|_| panic!("seed fits"))
                    .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
                    .unwrap();
                let invocation = ledger.prospective_invocation(TaskId::MAIN);
                let context = || {
                    CollectionRegion::new(exe, self, &exits, &ledger, &arena, &slots, (128, 128))
                };
                let cx = context();
                let completed = match step {
                    CollectionCase::Visit(_) => cx.tested(case == 0),
                    CollectionCase::Merge(_) if !tail && case == 0 => cx.exits.callback.clone(),
                    _ => cx.committed(),
                }
                .expect("the recognized exit has a canonical source successor");
                let exit = room::publication::Position {
                    at: completed.0,
                    unpaid: Some(completed.1),
                    recovery: [7; 32],
                    occurrence: room::publication::Occurrence {
                        task: TaskId::MAIN,
                        invocation: invocation.id(),
                        loops: vec![],
                        action: None,
                    },
                };
                let prepare = |exit| match step {
                    CollectionCase::Append(append) => append.prepare_region(context(), exit),
                    CollectionCase::Reduce(reduce) => reduce.prepare_region(context(), exit),
                    CollectionCase::Place(place) => place.prepare_region(context(), exit),
                    CollectionCase::Visit(visit) => visit.prepare_region(context(), exit),
                    CollectionCase::ReadSlot(read) => read.prepare_region(context(), exit),
                    CollectionCase::Merge(merge) if *tail => merge.prepare_tail(context(), exit),
                    CollectionCase::Merge(merge) => merge.prepare_head(context(), exit),
                };
                let mut wrong = exit.clone();
                wrong.at.phase = room::Phase::Enter;
                assert!(
                    prepare(wrong).is_none(),
                    "an unrelated exit is not selected"
                );
                let allocations = arena.allocations();
                let before = ledger.balance().unwrap();
                let facts = prepare(exit).expect("the actual finite kernel prepares");
                assert_eq!(
                    arena.allocations(),
                    allocations,
                    "preparation claims no source ordinal"
                );
                assert_eq!(ledger.balance().unwrap(), before);
                assert_eq!(facts.kind, room::ExitKind::Normal);
                match step {
                    CollectionCase::Place(place) => {
                        if let Some(at) = place.in_place {
                            assert!(facts.writes.iter().any(|(physical, value)| *physical == at
                                && value.same(&Value::Bool(true))));
                        }
                    }
                    CollectionCase::Reduce(reduce) => {
                        assert!(
                            facts
                                .writes
                                .iter()
                                .any(|(at, value)| *at == reduce.accumulator
                                    && value.same(&Value::Int(18)))
                        );
                    }
                    CollectionCase::ReadSlot(read) => {
                        assert_eq!(facts.transfer.pc, self.blocks[read.arms[1]].end);
                        assert_eq!(
                            facts.transfer.blocks,
                            vec![(read.body, 3), (read.arms[0], 1), (read.arms[1], 2)]
                        );
                        assert!(facts.writes.iter().any(|(at, value)| *at == read.present
                            && value.same(&Value::Bool(case != 0))));
                    }
                    CollectionCase::Visit(visit) => {
                        assert_eq!(facts.transfer.pc, visit.after[usize::from(case != 0)]);
                        assert_eq!(facts.branches.last().unwrap().1, case == 0);
                    }
                    CollectionCase::Merge(merge) if *tail => {
                        assert!(!facts.writes.iter().any(|(at, _)| merge.items.contains(at)));
                        assert_eq!(
                            facts.transfer.leave,
                            Some([merge.compared[1], merge.compared[0]])
                        );
                        assert_eq!(facts.transfer.resume, Some((merge.body, 5)));
                        assert_eq!(
                            facts.transfer.clears.len(),
                            self.blocks[merge.compared[0]].declares.len()
                                + self.blocks[merge.compared[1]].declares.len()
                        );
                        assert!(facts.member.as_ref().unwrap().2.same(&hole));
                    }
                    CollectionCase::Merge(merge) if case == 0 => {
                        assert!(facts.member.is_none());
                        assert_eq!(
                            facts.transfer.continuation,
                            CollectionContinuation::Callback
                        );
                        assert_eq!(facts.transfer.pc, merge.callback);
                        assert!(
                            facts
                                .writes
                                .iter()
                                .any(|(at, value)| *at == merge.items[1]
                                    && value.same(&Value::Absent))
                        );
                    }
                    _ => {}
                }
                let selected_exit = facts.exit.clone();
                reached.push(step.family());
                if let CollectionCase::Append(append) = step {
                    // K-MACH-008: rejection, abandonment and a stale *input*
                    // must not edit the target, its edges or its source IDs.
                    let out_handle = out.handle().unwrap();
                    let length = arena.len(out_handle).unwrap();
                    let entry = Dependencies::capture(&ledger, &arena, &[]).unwrap();
                    let CollectionBoundary {
                        transaction: boundary,
                        storage,
                        transfer,
                        ..
                    } = facts
                        .prepare_boundary(
                            &ledger,
                            &mut arena,
                            heap,
                            &self.source_room,
                            &invocation,
                            &[],
                            &[],
                            entry,
                        )
                        .unwrap()
                        .expect("append storage and membership stage together");
                    let peak = boundary.peak();
                    assert!(boundary.admit(peak - 1).is_err());
                    drop(storage);
                    assert!(transfer.writes.is_empty());
                    assert_eq!(arena.len(out_handle).unwrap(), length);
                    assert_eq!(arena.allocations(), allocations);
                    assert_eq!(ledger.balance().unwrap(), before);
                    let context = || {
                        CollectionRegion::new(
                            exe,
                            self,
                            &exits,
                            &ledger,
                            &arena,
                            &slots,
                            (128, 128),
                        )
                    };
                    let completed = context().committed().unwrap();
                    let exit = room::publication::Position {
                        at: completed.0,
                        unpaid: Some(completed.1),
                        recovery: [7; 32],
                        occurrence: room::publication::Occurrence {
                            task: TaskId::MAIN,
                            invocation: invocation.id(),
                            loops: vec![],
                            action: None,
                        },
                    };
                    let facts = append.prepare_region(context(), exit.clone()).unwrap();
                    let entry = Dependencies::capture(&ledger, &arena, &[]).unwrap();
                    let CollectionBoundary {
                        transaction: boundary,
                        storage,
                        ..
                    } = facts
                        .prepare_boundary(
                            &ledger,
                            &mut arena,
                            heap,
                            &self.source_room,
                            &invocation,
                            &[],
                            &[],
                            entry,
                        )
                        .unwrap()
                        .unwrap();
                    drop(boundary);
                    drop(storage);
                    assert_eq!(arena.len(out_handle).unwrap(), length);
                    let facts = append
                        .prepare_region(
                            CollectionRegion::new(
                                exe,
                                self,
                                &exits,
                                &ledger,
                                &arena,
                                &slots,
                                (128, 128),
                            ),
                            exit.clone(),
                        )
                        .unwrap();
                    let entry = Dependencies::capture(&ledger, &arena, &[]).unwrap();
                    let CollectionBoundary {
                        transaction: boundary,
                        storage,
                        ..
                    } = facts
                        .prepare_boundary(
                            &ledger,
                            &mut arena,
                            heap,
                            &self.source_room,
                            &invocation,
                            &[],
                            &[],
                            entry,
                        )
                        .unwrap()
                        .unwrap();
                    boundary
                        .admit(u64::MAX)
                        .unwrap_or_else(|_| panic!("append fits"))
                        .publish(&mut ledger, |arena, edits| storage.publish(arena, edits))
                        .unwrap();
                    assert_eq!(arena.len(out_handle).unwrap(), length + 1);
                    let SlotState::Value(item) = slots[append.item] else {
                        unreachable!()
                    };
                    assert!(arena.item(out_handle, length).unwrap().unwrap().same(&item));
                    assert_eq!(arena.allocations(), allocations);
                    let facts = append
                        .prepare_region(
                            CollectionRegion::new(
                                exe,
                                self,
                                &exits,
                                &ledger,
                                &arena,
                                &slots,
                                (128, 128),
                            ),
                            exit,
                        )
                        .unwrap();
                    let entry = Dependencies::capture(&ledger, &arena, &[]).unwrap();
                    // Stale discarded input evidence is still required even
                    // when no binding from this kernel survives the exit.
                    arena
                        .list_set(item.handle().unwrap(), 0, Value::Int(99))
                        .unwrap();
                    assert!(
                        facts
                            .prepare_boundary(
                                &ledger,
                                &mut arena,
                                heap,
                                &self.source_room,
                                &invocation,
                                &[],
                                &[],
                                entry,
                            )
                            .unwrap()
                            .is_none()
                    );
                }
                self.check_direct_collection_entry(
                    exe,
                    heap,
                    step,
                    *tail,
                    &mut arena,
                    &slots,
                    selected_exit,
                );
            }
        }
        reached
    }
}

impl Compiled {
    #[expect(
        clippy::too_many_arguments,
        reason = "the existing collection preparation fixture"
    )]
    fn check_direct_collection_entry(
        &self,
        exe: &Executable,
        heap: &PagedHeap,
        step: &CollectionCase<'_>,
        tail: bool,
        arena: &mut Arena,
        slots: &[SlotState],
        mut exit: room::publication::Position,
    ) {
        use direct::*;
        let placement = self.placement.as_ref().expect("source binding placement");
        let physical: Vec<_> = (0..slots.len()).collect();
        let logical = self
            .source_room
            .collection_bindings(placement, &physical)
            .expect("the fixture has unshared source bindings");
        let bindings: Vec<_> = logical
            .iter()
            .zip(slots)
            .filter_map(|(slot, state)| match state {
                SlotState::Value(value) => Some((*slot, RootValue::Value(value))),
                SlotState::Empty => None,
                SlotState::Cell(_) => panic!("no shared fixture binding"),
            })
            .collect();
        let mut ledger = Ledger::fresh();
        let (invocation, boundary) = ledger
            .prepare_enter_publication(
                TaskId::MAIN,
                arena.registration().unwrap(),
                heap.pages(),
                &[],
                &bindings,
            )
            .unwrap();
        boundary
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("fixture entry fits"))
            .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
            .unwrap();
        exit.occurrence.invocation = invocation.id();
        let activation = placement
            .capture(placement.code(), &invocation, &ledger, heap, slots)
            .unwrap()
            .expect("actual source activation");
        let entry = |exit| CollectionEntry {
            activation: &activation,
            invocation: &invocation,
            exit,
        };
        macro_rules! check {
            ($site:ty, $shape:expr, $physical:expr, $replacement:expr) => {{
                let site = <$site>::compile(exe, self, $shape).expect("typed recognized collection site");
                let run = |slots, exit| site.prepare(
                    exe, self, &ledger, arena, slots, (128, 128), entry(exit),
                );
                let facts = run(slots, exit.clone()).expect("source-bound direct preparation");
                let mut wrong = slots.to_vec();
                wrong[$physical] = SlotState::Value($replacement);
                assert!(run(&wrong, exit.clone()).is_none(),
                    "a direct collection cannot replace an actual source input with a same-price value");
                let mut wrong_exit = exit.clone();
                wrong_exit.unpaid = Some(wrong_exit.at.clone());
                assert!(run(slots, wrong_exit).is_none(), "completed work is not an unpaid exit");
                let stale = run(slots, exit.clone()).expect("independent candidate before abandonment");
                (facts, stale)
            }};
        }
        let (facts, stale) = match step {
            CollectionCase::Append(shape) => check!(AppendSite, shape, shape.item, Value::Int(97)),
            CollectionCase::Reduce(shape) => {
                check!(ReduceSite, shape, shape.accumulator, Value::Int(97))
            }
            CollectionCase::Place(shape) => {
                check!(PlacementSite, shape, shape.value, Value::Int(97))
            }
            CollectionCase::Visit(shape) => check!(VisitSite, shape, shape.index, Value::Int(1)),
            CollectionCase::ReadSlot(shape) => check!(
                ReadSlotSite,
                shape,
                shape.index,
                Value::Int(
                    if matches!(slots[shape.index], SlotState::Value(Value::Int(0))) {
                        1
                    } else {
                        0
                    }
                )
            ),
            CollectionCase::Merge(shape) if tail => {
                check!(MergeTailSite, shape, shape.order, Value::Int(97))
            }
            CollectionCase::Merge(shape) => {
                check!(MergeSite, shape, shape.counters[5], Value::Int(2))
            }
        };
        let program = facts
            .program
            .as_ref()
            .expect("typed facts retain the cached source program");
        assert_eq!(program.verify(exe), Ok(()));
        let mut unpaid = facts.exit.clone();
        unpaid.unpaid = Some(unpaid.at.clone());
        assert!(
            !program.matches(&unpaid, &facts.branches),
            "a completed event cannot become the next unpaid head"
        );
        let event_points: Vec<_> = program
            .events()
            .iter()
            .flat_map(|interval| interval.events().map(|event| event.point.clone()))
            .collect();
        let close_positions: Vec<_> = program
            .charges()
            .map(|charges| {
                event_points
                    .iter()
                    .position(|point| point == charges.first())
                    .expect("every charged span starts in this actual source prefix")
            })
            .collect();
        assert!(
            close_positions.windows(2).all(|pair| pair[0] < pair[1]),
            "canonical statement closes keep the independent source event order"
        );
        let source_scopes: Vec<_> = event_points
            .iter()
            .filter(|point| matches!(point.phase, room::Phase::Enter | room::Phase::Leave))
            .cloned()
            .collect();
        assert_eq!(
            program.scopes(),
            source_scopes,
            "scope controls agree with the original source events"
        );
        let compile = || {
            room::CollectionProgram::compile_paths(
                exe,
                placement.code(),
                program.first().clone(),
                program.through().clone(),
            )
            .unwrap()
            .into_iter()
            .find(|path| path.choices() == program.choices())
            .unwrap()
        };
        let mut wrong = compile();
        wrong.replace_peak_with_final_for_law();
        assert_eq!(
            wrong.verify(exe),
            Err(room::Refused::Peak),
            "final holdings cannot replace the source interior peak"
        );
        let mut wrong = compile();
        wrong.omit_scope_for_law();
        assert_eq!(
            wrong.verify(exe),
            Err(room::Refused::Source),
            "source Enter/Leave controls cannot disappear from a collection prefix"
        );
        let mut wrong = compile();
        wrong.corrupt_charge_for_law();
        assert_eq!(
            wrong.verify(exe),
            Err(room::Refused::Exit),
            "every original close remains in source order"
        );
        if matches!(step, CollectionCase::Visit(_) | CollectionCase::ReadSlot(_)) {
            assert!(
                !program.allocations().is_empty(),
                "hole predicates retain the real source empty-tuple construction obligation"
            );
        }
        if program.through().phase == room::Phase::Test {
            let branches: Vec<_> = facts
                .branches
                .iter()
                .filter(|(site, _)| *site != program.through().site)
                .cloned()
                .collect();
            assert!(
                !program.matches(&facts.exit, &branches),
                "the final test cannot lose its selected successor"
            );
        }
        if !program.allocations().is_empty() {
            let mut wrong = compile();
            wrong.omit_allocation_for_law();
            assert_eq!(
                wrong.verify(exe),
                Err(room::Refused::Source),
                "empty and hole arms retain their source allocation ordinals"
            );
        }
        if !facts.branches.is_empty() {
            let mut branches = facts.branches.clone();
            branches[0].1 = !branches[0].1;
            assert!(
                !program.matches(&facts.exit, &branches),
                "same-priced opposite branches cannot select this source exit"
            );
        }
        let stale_target = facts.member.map(|(handle, _, _)| handle);
        let writes = facts.writes.clone();
        let clears = facts.transfer.clears.clone();
        let allocations = arena.allocations();
        let balance = ledger.balance().unwrap();
        let CollectionBoundary {
            mut transaction,
            storage,
            ..
        } = facts
            .prepare_direct_boundary(&ledger, arena, heap)
            .unwrap()
            .expect("source-bound detached boundary");
        let outputs: Vec<_> = writes
            .iter()
            .map(|(physical, value)| {
                (
                    invocation.owner(logical[*physical]),
                    RootValue::Value(value),
                )
            })
            .collect();
        let inputs: Vec<_> = clears
            .iter()
            .copied()
            .chain(writes.iter().map(|(physical, _)| *physical))
            .map(|physical| invocation.owner(logical[physical]))
            .collect();
        assert!(transaction.matches_roots(&ledger, &inputs, &outputs));
        if let Some((owner, _)) = outputs.first() {
            assert!(
                !transaction.matches_roots(
                    &ledger,
                    &inputs,
                    &[(*owner, RootValue::Value(&Value::Int(999)))]
                ),
                "a same-priced wrong result cannot replace the bound direct transfer"
            );
        }
        drop(transaction);
        drop(storage);
        assert_eq!(
            arena.allocations(),
            allocations,
            "abandonment claims no ordinal"
        );
        assert_eq!(
            ledger.balance().unwrap(),
            balance,
            "abandonment publishes no source owner"
        );
        if let Some(handle) = stale_target {
            arena.list_set(handle, 0, Value::Int(203)).unwrap();
        } else {
            ledger.handoff().unwrap();
        }
        assert!(
            stale
                .prepare_direct_boundary(&ledger, arena, heap)
                .unwrap()
                .is_none(),
            "a detached collection rejects entry dependencies that changed before publication"
        );
        // Establish this law's explicit source entry separately from the live
        // packet header. No synthetic receipt substitutes for the runtime gate.
        let mut initial = slots.to_vec();
        match step {
            CollectionCase::ReadSlot(shape) => {
                initial[shape.value] = SlotState::Empty;
                initial[shape.present] = SlotState::Empty;
            }
            CollectionCase::Place(shape) => {
                if let Some(at) = shape.in_place {
                    initial[at] = SlotState::Empty;
                }
            }
            CollectionCase::Merge(shape) if !tail => {
                initial[shape.from_right] = SlotState::Empty;
                for block in shape.compared {
                    for at in &self.blocks[block].declares {
                        initial[*at as usize] = SlotState::Empty;
                    }
                }
            }
            _ => {}
        }
        macro_rules! accepted {
            ($site:ty, $shape:expr) => {
                self.check_affine_collection(
                    exe,
                    heap,
                    arena,
                    &initial,
                    &<$site>::compile(exe, self, $shape).unwrap(),
                    exit,
                )
            };
        }
        match step {
            CollectionCase::Append(shape) => accepted!(AppendSite, shape),
            CollectionCase::Reduce(shape) => accepted!(ReduceSite, shape),
            CollectionCase::Place(shape) => accepted!(PlacementSite, shape),
            CollectionCase::Visit(shape) => accepted!(VisitSite, shape),
            CollectionCase::ReadSlot(shape) => accepted!(ReadSlotSite, shape),
            CollectionCase::Merge(shape) if tail => accepted!(MergeTailSite, shape),
            CollectionCase::Merge(shape) => accepted!(MergeSite, shape),
        }
    }
}

/// Only the fixture uses a family sum to generate its independent cases.
enum CollectionCase<'a> {
    Append(&'a CollectionAppend),
    Reduce(&'a Reduction),
    Place(&'a Placement),
    Visit(&'a Visit),
    ReadSlot(&'a ReadSlot),
    Merge(&'a Merge),
}
impl CollectionCase<'_> {
    fn lowering(&self) -> CollectionStep {
        match self {
            Self::Append(shape) => CollectionStep::Append((*shape).clone()),
            Self::Reduce(shape) => CollectionStep::Reduce((*shape).clone()),
            Self::Place(shape) => CollectionStep::Place((*shape).clone()),
            Self::Visit(shape) => CollectionStep::Visit((*shape).clone()),
            Self::ReadSlot(shape) => CollectionStep::ReadSlot((*shape).clone()),
            Self::Merge(shape) => CollectionStep::Merge((*shape).clone()),
        }
    }
    fn family(&self) -> Family {
        self.lowering().family()
    }
}
