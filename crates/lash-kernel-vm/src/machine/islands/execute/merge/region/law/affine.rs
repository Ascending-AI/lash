//! Component acceptance is separate from the mandatory live packet witness.
use super::*;
impl Compiled {
    pub(super) fn check_affine_collection<S>(
        &self,
        exe: &Executable,
        heap: &PagedHeap,
        arena: &mut Arena,
        slots: &[SlotState],
        site: &direct::CollectionSite<S>,
        mut selected: room::publication::Position,
    ) where
        direct::CollectionSite<S>: direct::CollectionCandidate,
    {
        use direct::*;
        let placement = self.placement.as_ref().unwrap();
        let physical: Vec<_> = (0..slots.len()).collect();
        let logical = self
            .source_room
            .collection_bindings(placement, &physical)
            .unwrap();
        let roots: Vec<_> = logical
            .iter()
            .zip(slots)
            .filter_map(|(slot, value)| match value {
                SlotState::Value(value) => Some((*slot, RootValue::Value(value))),
                _ => None,
            })
            .collect();
        if let Some(first) = slots.iter().find_map(|slot| match slot {
            SlotState::Value(value @ Value::List(_)) => Some(*value),
            _ => None,
        }) {
            let other = arena.create_list(&[]);
            assert!(
                !room::CollectionProgram::alias_matches_for_law(arena, heap, first, other).unwrap(),
                "same-priced mutable references retain their actual source identities"
            );
        }
        let store = heap.logical_ledger();
        let saved = std::mem::replace(&mut *store.borrow_mut(), Ledger::fresh());
        let mut ledger = store.borrow_mut();
        let (invocation, entered) = ledger
            .prepare_enter_publication(
                TaskId::MAIN,
                arena.registration().unwrap(),
                heap.pages(),
                &[],
                &roots,
            )
            .unwrap();
        entered
            .admit(u64::MAX)
            .ok()
            .unwrap()
            .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
            .unwrap();
        drop(ledger);
        let ledger = store.borrow();
        selected.occurrence.invocation = invocation.id();
        let dense = placement
            .capture(placement.code(), &invocation, &ledger, heap, slots)
            .unwrap()
            .unwrap();
        let exit = site
            .positions(selected.occurrence.clone())
            .find(|exit| exit.at == selected.at && exit.unpaid == selected.unpaid)
            .expect("the concrete native source path has an affine publication plan");
        let run = |arena: &Arena| {
            site.candidate(
                CandidateContext {
                    exe,
                    compiled: self,
                    ledger: &ledger,
                    arena,
                    slots,
                    limits: (128, 128),
                },
                CollectionEntry {
                    activation: &dense,
                    invocation: &invocation,
                    exit: exit.clone(),
                },
            )
            .unwrap()
        };
        let candidate = run(arena);
        if candidate.transfer.continuation == CollectionContinuation::Callback {
            drop(ledger);
            *store.borrow_mut() = saved;
            return;
        }
        let start = candidate.publication_start().unwrap();
        let mut frontier = self
            .source_room
            .frontier(
                &ledger,
                heap,
                &invocation,
                lash_kernel_value::content_ids::LinkId::from_bytes([71; 32]),
                lash_kernel_doc::KernelVersion::of(1).unwrap(),
                start,
            )
            .unwrap();
        fn resources<'a>(
            charged: &'a mut u64,
            counters: &'a mut crate::machine::tasks::Occurrences,
        ) -> crate::admission::SourceResources<'a> {
            crate::admission::SourceResources {
                charged,
                counters,
                bound: u64::MAX,
                slice: u64::MAX,
                depth: 1,
                limit: u64::MAX,
                safe_point: 0,
            }
        }
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
        let abandoned_allocations = arena.allocations();
        let abandoned_balance = ledger.balance().unwrap();
        let mut wrong = run(arena);
        if let Some((_, value)) = wrong.writes.first_mut() {
            *value = Value::Int(999);
            assert!(
                wrong
                    .accept(
                        exe,
                        &frontier,
                        &dense,
                        &ledger,
                        heap,
                        arena,
                        resources(&mut charged, &mut counters),
                        u64::MAX
                    )
                    .unwrap()
                    .is_none(),
                "source verification rejects a same-price wrong result before storage publication"
            );
        }
        assert_eq!(arena.allocations(), abandoned_allocations);
        assert_eq!(ledger.balance().unwrap(), abandoned_balance);
        assert_eq!(charged, 0);
        let member = candidate.member;
        let before = arena.allocations();
        let (accepted, storage, _transfer) = candidate
            .accept(
                exe,
                &frontier,
                &dense,
                &ledger,
                heap,
                arena,
                resources(&mut charged, &mut counters),
                u64::MAX,
            )
            .unwrap()
            .expect("actual source facts join the detached transaction under affine acceptance");
        drop(ledger);
        accepted
            .publish_boundary(
                &mut frontier,
                &mut store.borrow_mut(),
                |arena, edits| storage.publish(arena, edits),
                None,
            )
            .unwrap();
        assert_eq!(frontier.position(), &exit);
        if let Some((handle, index, value)) = member {
            assert!(arena.item(handle, index).unwrap().unwrap().same(&value));
        }
        assert_eq!(
            arena.allocations(),
            before,
            "the live empty tuple is cached, not a fresh entity ordinal"
        );
        *store.borrow_mut() = saved;
    }
}
