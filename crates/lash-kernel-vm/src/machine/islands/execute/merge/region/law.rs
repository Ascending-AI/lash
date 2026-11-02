//! Local preparation proof. Full source/charge acceptance is core's gate.
use super::*;
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
        for (step, tail) in &self.code.sites.collection {
            let exits = CollectionExits::compile(step, exe, self)
                .expect("source exits compile with the finite site");
            for case in 0..3 {
                let mut arena = Arena::new();
                let hole = arena.tuple(vec![], 128).unwrap();
                let sorted = arena.create_list(&[hole, Value::Absent]);
                let out = arena.create_list(&[Value::Int(3), Value::Int(7)]);
                let mut slots = vec![SlotState::Value(Value::Int(0)); count];
                let mut set = |at, value| slots[at] = SlotState::Value(value);
                match &**step {
                    CollectionStep::Append(append) => {
                        set(append.target, out);
                        set(append.item, if case == 1 { out } else { sorted });
                    }
                    CollectionStep::Reduce(reduce) => {
                        set(reduce.accumulator, Value::Int(11));
                        set(reduce.item, Value::Int(7));
                    }
                    CollectionStep::Place(place) => {
                        set(place.target, out);
                        set(place.index, Value::Int(0));
                        set(place.value, sorted);
                        if let Some(list) = place.list {
                            set(list, Value::Bool(true));
                        }
                    }
                    CollectionStep::Visit(visit) => {
                        set(visit.index, Value::Int(0));
                        set(visit.length, Value::Int(2));
                        set(visit.value, if case == 0 { hole } else { Value::Absent });
                    }
                    CollectionStep::ReadSlot(read) => {
                        set(read.target, sorted);
                        set(read.index, Value::Int(i64::from(case != 0)));
                        set(read.list, Value::Bool(true));
                    }
                    CollectionStep::Merge(merge) => {
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
                let completed = match &**step {
                    CollectionStep::Visit(_) => cx.tested(case == 0),
                    CollectionStep::Merge(_) if !tail && case == 0 => cx.exits.callback.clone(),
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
                let prepare = |exit| match &**step {
                    CollectionStep::Append(append) => append.prepare_region(context(), exit),
                    CollectionStep::Reduce(reduce) => reduce.prepare_region(context(), exit),
                    CollectionStep::Place(place) => place.prepare_region(context(), exit),
                    CollectionStep::Visit(visit) => visit.prepare_region(context(), exit),
                    CollectionStep::ReadSlot(read) => read.prepare_region(context(), exit),
                    CollectionStep::Merge(merge) if *tail => merge.prepare_tail(context(), exit),
                    CollectionStep::Merge(merge) => merge.prepare_head(context(), exit),
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
                match &**step {
                    CollectionStep::Place(place) => {
                        if let Some(at) = place.in_place {
                            assert!(facts.writes.iter().any(|(physical, value)| *physical == at
                                && value.same(&Value::Bool(true))));
                        }
                    }
                    CollectionStep::Reduce(reduce) => {
                        assert!(
                            facts
                                .writes
                                .iter()
                                .any(|(at, value)| *at == reduce.accumulator
                                    && value.same(&Value::Int(18)))
                        );
                    }
                    CollectionStep::ReadSlot(read) => {
                        assert_eq!(facts.transfer.pc, self.blocks[read.arms[1]].end);
                        assert_eq!(
                            facts.transfer.blocks,
                            vec![(read.body, 3), (read.arms[0], 1), (read.arms[1], 2)]
                        );
                        assert!(facts.writes.iter().any(|(at, value)| *at == read.present
                            && value.same(&Value::Bool(case != 0))));
                    }
                    CollectionStep::Visit(visit) => {
                        assert_eq!(facts.transfer.pc, visit.after[usize::from(case != 0)]);
                        assert_eq!(facts.branches.last().unwrap().1, case == 0);
                    }
                    CollectionStep::Merge(merge) if *tail => {
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
                    CollectionStep::Merge(merge) if case == 0 => {
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
                reached.push(step.family());
                if let CollectionStep::Append(append) = &**step {
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
            }
        }
        reached
    }
}
