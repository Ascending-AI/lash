//! FIG-7614's native boundary law runs inside helper_frames_agree.
use super::*;
use crate::machine::source_publication::native_storage::{NativeBoundary, NativeStorage};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::logical::{Ledger, Owner, ScratchEvent, Slot};
use lash_kernel_value::sites::{Site, Unit};
use std::{cell::RefCell, rc::Rc};

impl KernelMachine {
    pub(crate) fn assert_native_boundary_law(&self, id: lash_kernel_value::FunctionId) {
        let function = self.exe.lib(self.exe.library_lib(&id).unwrap());
        let LibRun::Native { native, .. } = &function.run else {
            panic!("installed native")
        };
        let ledger = Rc::new(RefCell::new(Ledger::fresh()));
        let mut arena = Arena::new();
        let owner = |index| Owner::Value {
            task: doc::TaskId::MAIN,
            invocation: 1,
            slot: Slot::Operand(index),
        };
        let input = doc::Value::Int(0);
        let initial = ledger
            .borrow()
            .prepare_source_boundary(
                arena.registration().unwrap(),
                &self.heap,
                &[],
                &[(
                    owner(0),
                    lash_kernel_state::kernel_internals::logical::RootValue::Value(&input),
                )],
            )
            .unwrap()
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("input fits"));
        initial
            .publish(&mut ledger.borrow_mut(), |_, _| {})
            .unwrap();
        let at = Point {
            site: Site::new(Unit::Main, vec![0]),
            phase: Phase::Commit,
        };
        let recipe = |mode, contract| NativeStorage {
            contract,
            function,
            args: vec![doc::Value::Int(mode)],
            output: owner(1),
            at: at.clone(),
            release: vec![].into_boxed_slice(),
            ledger: Rc::clone(&ledger),
            heap: &self.heap,
            charging: true,
            limits: self.limits,
            room: None,
            known: None,
        };
        let contract = |mode, arena: &Arena| {
            native
                .resource_contract(doc::Args::new(
                    &[doc::Value::Int(mode)],
                    arena.objects(),
                    &arena.semantics(),
                ))
                .unwrap()
                .unwrap()
        };
        let mut wrong = recipe(0, contract(0, &arena));
        wrong.known = Some((doc::Value::Int(8), 0));
        assert!(
            wrong
                .prepare_boundary(&mut arena, &[owner(0)])
                .unwrap()
                .is_none(),
            "same-price native result must match its canonical prepared entry"
        );
        let mut exact = recipe(0, contract(0, &arena));
        exact.known = Some((doc::Value::Int(7), 0));
        let exact = exact
            .prepare_boundary(&mut arena, &[owner(0)])
            .unwrap()
            .unwrap();
        assert!(matches!(exact.storage.value(), doc::Value::Int(7)));
        drop(exact);
        let balance = ledger.borrow().balance().unwrap();
        let identity = arena.next_provisional();
        let allocations = arena.allocations();
        // Changed result bytes, a dishonest graph and an unknown input contract
        // must leave both the ledger and the physical nursery untouched.
        assert!(
            recipe(2, contract(2, &arena))
                .prepare_boundary(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert!(
            recipe(3, contract(3, &arena))
                .prepare_boundary(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert!(
            recipe(4, contract(0, &arena))
                .prepare_boundary(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert!(
            recipe(5, contract(5, &arena))
                .prepare_boundary(&mut arena, &[owner(0)])
                .unwrap()
                .is_none()
        );
        assert_eq!(ledger.borrow().balance().unwrap(), balance);
        assert_eq!(arena.next_provisional(), identity);
        assert_eq!(arena.allocations(), allocations);
        let scalar = recipe(0, contract(0, &arena))
            .prepare_boundary(&mut arena, &[owner(0)])
            .unwrap()
            .expect("scalar native ran");
        assert!(matches!(scalar.storage.value(), doc::Value::Int(7)));
        assert_eq!(scalar.storage.units(), 4);
        assert_eq!(scalar.transaction.peak(), 17);
        drop(scalar);
        // The graph call's actual nonzero input has a one-byte magnitude.
        let list_input = doc::Value::Int(1);
        let update = ledger
            .borrow()
            .prepare_source_boundary(
                arena.registration().unwrap(),
                &self.heap,
                &[owner(0)],
                &[(
                    owner(0),
                    lash_kernel_state::kernel_internals::logical::RootValue::Value(&list_input),
                )],
            )
            .unwrap()
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("nonzero input fits"));
        update.publish(&mut ledger.borrow_mut(), |_, _| {}).unwrap();
        let balance = ledger.borrow().balance().unwrap();
        let phases = contract(1, &arena);
        let events = native_storage::scratch_events(owner(1), &phases).unwrap();
        let scratch = |index| Owner::Value {
            task: doc::TaskId::MAIN,
            invocation: 1,
            slot: Slot::Scratch(index),
        };
        assert!(matches!(events.as_slice(), [
            ScratchEvent::Begin { owner: a, bytes: 40 }, ScratchEvent::End { owner: b },
            ScratchEvent::Begin { owner: c, bytes: 20 }, ScratchEvent::Begin { owner: d, bytes: 30 },
            ScratchEvent::End { owner: e }, ScratchEvent::End { owner: f },
        ] if *a == scratch(0) && *b == scratch(0) && *c == scratch(1)
            && *d == scratch(2) && *e == scratch(2) && *f == scratch(1)));
        assert_eq!(phases.peak_scratch_bytes(), Some(50));
        // 17-byte input + a 66-byte list + the 20-byte lease still live
        // during the entity allocation. The 30-byte lease ended beforehand.
        let peak = balance.total().unwrap() + 86;
        let NativeBoundary {
            transaction,
            storage,
        } = recipe(1, phases.clone())
            .prepare_boundary(&mut arena, &[owner(0)])
            .unwrap()
            .expect("actual native boundary executed");
        assert_eq!(transaction.peak(), peak);
        assert_eq!(storage.work(), 2);
        assert_eq!(storage.units(), 6);
        assert!(matches!(
            storage.formula_values().arguments,
            [doc::Value::Int(1)]
        ));
        assert_eq!(
            storage.measure(crate::compile::Source::Result, doc::Measure::Size),
            Some(3)
        );
        assert!(transaction.admit(peak - 1).is_err());
        drop(storage);
        assert_eq!(arena.next_provisional(), identity);
        assert_eq!(arena.allocations(), allocations);
        let NativeBoundary {
            transaction,
            storage,
        } = recipe(1, phases)
            .prepare_boundary(&mut arena, &[owner(0)])
            .unwrap()
            .expect("retry runs canonical native");
        let value = storage.value();
        transaction
            .admit(peak)
            .unwrap_or_else(|_| panic!("exact native peak fits"))
            .publish(&mut ledger.borrow_mut(), |arena, edits| {
                assert!(edits.is_empty());
                storage.publish(arena);
            })
            .unwrap();
        let doc::Value::List(handle) = value else {
            panic!("canonical list result")
        };
        assert_eq!(arena.len(handle).unwrap(), 2);
        assert_eq!(arena.next_provisional().ordinal, identity.ordinal + 1);
        assert_eq!(ledger.borrow().balance().unwrap().total().unwrap(), 82);
        assert!(
            ledger
                .borrow()
                .holding(owner(0), self.heap.pages())
                .unwrap()
                .is_none()
        );
        assert!(
            ledger
                .borrow()
                .holding(owner(1), self.heap.pages())
                .unwrap()
                .is_some()
        );
        println!(
            "native boundary ABI executed: changed result/body/unknown/refusal rejected; ordered scratch peak {peak}; 1 graph published"
        );
    }
}
