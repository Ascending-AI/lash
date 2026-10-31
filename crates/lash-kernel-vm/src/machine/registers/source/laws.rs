use super::*;
use crate::Machine;
use crate::compile::{Form, SourceJoin};
use lash_kernel_exec::{ConstIx, OpRange, RegRange};
use lash_kernel_value::sites::Unit;

/// K-BND-003 / K-EVAL-008: source occurrences own their evaluated values;
/// clearing a register mirror neither changes a role nor creates Null owners.
#[test]
fn source_operands_follow_consumption_and_ignore_register_mirrors() {
    let run = crate::laws::embedder::Embedder::new("main { return null }");
    let projection = Projection {
        arena: &run.machine.arena,
        heap: &run.machine.heap,
    };
    let root = Site::new(Unit::Main, vec![0]);
    let selected = Arc::new(root.child(0));
    let leaf = Arc::new(selected.child(1));
    let sibling = Arc::new(root.child(1));
    let result = Arc::new(root.clone());
    let stream = Stream {
        ops: vec![
            Op::Const {
                dst: Reg(19),
                c: ConstIx(0),
            },
            Op::Const {
                dst: Reg(3),
                c: ConstIx(0),
            },
            Op::MakeTuple {
                items: RegRange {
                    start: Reg(0),
                    len: 0,
                },
            },
        ],
        source_sites: vec![
            Some(Arc::clone(&leaf)),
            Some(Arc::clone(&sibling)),
            Some(Arc::clone(&result)),
        ],
        source_joins: std::collections::BTreeMap::from([(
            0,
            vec![SourceJoin {
                from: Arc::clone(&leaf),
                to: Arc::clone(&selected),
                value: Reg(19),
            }],
        )]),
        ..Default::default()
    };
    let stmt = Stmt {
        site: root.clone(),
        ordinal: None,
        ops: OpRange { start: 0, len: 3 },
        regs: 64,
        form: Form::Plain,
        source_roles: std::sync::Mutex::new(Default::default()),
    };
    let mut registers = vec![Value::Null; 64];
    let mut owners = Operands::default();
    owners
        .begin(&stream, &stmt, 0, &registers, &projection)
        .unwrap();
    assert!(owners.is_empty(), "unwritten Null storage is not a holding");
    registers[19] = Value::Int(7);
    owners
        .complete(&stream, 0, &registers, &projection)
        .unwrap();
    registers[19] = Value::Null;
    assert!(
        owners
            .prepare_join(&SourceJoin {
                from: Arc::clone(&leaf),
                to: Arc::clone(&result),
                value: Reg(19)
            })
            .is_err(),
        "an accepted join cannot rename a receipt from its spent child site"
    );
    assert!(
        owners.take(Reg(19), &leaf).is_err(),
        "the branch owner was transferred"
    );
    assert!(matches!(
        owners.take(Reg(19), &selected).map(|held| held.value),
        Ok(Value::Int(7))
    ));
    assert!(owners.is_empty());

    registers[3] = Value::Int(11);
    owners
        .complete(&stream, 1, &registers, &projection)
        .unwrap();
    let consume = Stream {
        ops: vec![Op::Bind {
            local: lash_kernel_exec::LocalRef { slot: 0, at: 31 },
            src: Reg(3),
        }],
        source_sites: vec![Some(Arc::clone(&result))],
        ..Default::default()
    };
    owners
        .complete(&consume, 0, &registers, &projection)
        .unwrap();
    assert!(
        owners.is_empty(),
        "a consumed source operand cannot remain in a register owner"
    );
    assert!(
        matches!(registers[3], Value::Int(11)),
        "the physical mirror can remain initialized"
    );
    // A protocol's open call takes the iterable's source owner. ForIn
    // completion must acknowledge that transfer exactly once.
    let protocol = Stream {
        ops: vec![Op::ForIn { iterable: Reg(3) }],
        source_sites: vec![Some(Arc::clone(&result))],
        ..Default::default()
    };
    owners
        .complete(&stream, 1, &registers, &projection)
        .unwrap();
    let _ = owners.take_arguments(3, 1);
    assert!(
        owners.is_empty(),
        "a transferred operand is no longer a source holding"
    );
    owners
        .complete(&protocol, 0, &registers, &projection)
        .unwrap();
    assert!(
        owners
            .complete(&protocol, 0, &registers, &projection)
            .is_err(),
        "a source transfer cannot complete twice"
    );
    owners
        .complete(&stream, 1, &registers, &projection)
        .unwrap();
    let _ = owners.take_arguments(3, 1);
    assert!(
        owners
            .complete(&consume, 0, &registers, &projection)
            .is_err(),
        "an ordinary binding cannot consume a call transfer"
    );
    owners.clear();
    owners.active = true;
    assert!(
        owners
            .complete(&protocol, 0, &registers, &projection)
            .is_err(),
        "a transfer receipt cannot survive its source bank"
    );
    // K-EVAL-008: a prepared else result still passes through its compiler
    // join before an enclosing true arm joins. Const, Local and construction
    // outputs must all reach the same real outer source owner.
    for layout in [crate::Layout(0), crate::Layout(1)] {
        let mut joined = crate::laws::embedder::Embedder::try_over(
            "main { let run = fn(value) { return [if(true, if(false, 0, 2), 0), if(true, if(false, 0, value), 0), if(true, if(false, [], [7]), [])] } return apply run(7) }",
            crate::laws::embedder::Setup { layout, ..Default::default() },
            None,
        ).unwrap();
        joined.machine.force_source_execution();
        let expected = lash_kernel_doc::Datum::List(vec![
            crate::laws::embedder::int(2),
            crate::laws::embedder::int(7),
            lash_kernel_doc::Datum::List(vec![crate::laws::embedder::int(7)]),
        ]);
        assert!(
            matches!(joined.run_to_end(&[]), crate::End::Finished(ref end) if end.result == expected)
        );
    }
    let mut live = crate::laws::embedder::Embedder::new(
        "main {
if true {
let a = [1]
let b = a
return b
}
return null
}",
    );
    assert_eq!(
        live.machine
            .heap
            .logical_ledger()
            .borrow()
            .balance()
            .unwrap()
            .total()
            .unwrap(),
        64,
        "a logical frame enters before its first instruction"
    );
    let end = live.run_to_end(&[]);
    assert!(matches!(end, crate::End::Finished(_)));
    live.machine.logical_flush().unwrap();
    let balance = live
        .machine
        .heap
        .logical_ledger()
        .borrow()
        .balance()
        .unwrap();
    assert_eq!(balance.entities, 1, "runtime aliases own one entity body");
    assert_eq!(balance.entity_local_bytes, 49);
    assert_eq!(
        balance.root_and_control_bytes, 16,
        "return releases the logical frame and its bindings"
    );
    // A pending return is owned through finally and survives a cold handoff.
    let mut cleanup = crate::laws::embedder::Embedder::new(
        "main {
try { return [7] } finally { do perform 0(\"cleanup\") as Any }
}",
    );
    assert!(matches!(cleanup.run(), crate::Step::Parked(_)));
    cleanup.machine.logical_flush().unwrap();
    let held = cleanup
        .machine
        .heap
        .logical_ledger()
        .borrow()
        .balance()
        .unwrap();
    assert_eq!(
        held.entities, 1,
        "finally retains the pending returned list"
    );
    assert_eq!(held.entity_local_bytes, 49);
    assert_eq!(
        held.root_and_control_bytes, 80,
        "one frame and the pending departure own the parked prefix"
    );
    cleanup.reopen(crate::Layout::default(), false);
    assert_eq!(
        cleanup
            .machine
            .heap
            .logical_ledger()
            .borrow()
            .balance()
            .unwrap(),
        held
    );
    cleanup.deliver("cleanup");
    let delivered = cleanup
        .machine
        .heap
        .logical_ledger()
        .borrow()
        .balance()
        .unwrap();
    assert_eq!(
        delivered.root_and_control_bytes, 103,
        "the committed frozen text adds its complete 23-byte occurrence"
    );
    cleanup.reopen(crate::Layout::default(), false);
    assert_eq!(
        cleanup
            .machine
            .heap
            .logical_ledger()
            .borrow()
            .balance()
            .unwrap(),
        delivered
    );
    assert!(matches!(cleanup.run_to_end(&[]), crate::End::Finished(_)));
    cleanup.machine.logical_flush().unwrap();
    assert_eq!(
        cleanup
            .machine
            .heap
            .logical_ledger()
            .borrow()
            .balance()
            .unwrap()
            .root_and_control_bytes,
        16
    );

    let mut looping = crate::laws::embedder::Embedder::new(
        "main {
for x in [[7]] {
do perform 0(\"loop\") as Any
return x
}
return null
}",
    );
    assert!(matches!(looping.run(), crate::Step::Parked(_)));
    looping.machine.logical_flush().unwrap();
    let held = looping
        .machine
        .heap
        .logical_ledger()
        .borrow()
        .balance()
        .unwrap();
    assert_eq!(
        held.entities, 2,
        "the cold loop cursor reaches its original nested list"
    );
    assert_eq!(held.entity_local_bytes, 97);
    assert_eq!(
        held.root_and_control_bytes, 96,
        "frame, loop binding and cursor are distinct source owners"
    );
    looping.reopen(crate::Layout::default(), false);
    looping.deliver("loop");
    assert!(matches!(looping.run_to_end(&[]), crate::End::Finished(_)));
    looping.machine.logical_flush().unwrap();
    let held = looping
        .machine
        .heap
        .logical_ledger()
        .borrow()
        .balance()
        .unwrap();
    assert_eq!(
        held.entities, 1,
        "leaving the loop releases its cursor and outer list"
    );
    assert_eq!(held.entity_local_bytes, 49);
    assert_eq!(held.root_and_control_bytes, 16);
    let mut incoming = crate::laws::embedder::Embedder::new(
        "main {
if true {
let value = perform 0(\"payload\") as Any
return value
}
return null
}",
    );
    assert!(matches!(incoming.run(), crate::Step::Parked(_)));
    let wait = crate::laws::embedder::wait_of(incoming.requests.last().unwrap());
    incoming
        .machine
        .deliver(
            wait,
            crate::Outcome::Completed(lash_kernel_doc::Datum::List(vec![
                crate::laws::embedder::int(1),
                crate::laws::embedder::int(2),
            ])),
        )
        .unwrap();
    let frozen = incoming
        .machine
        .heap
        .logical_ledger()
        .borrow()
        .balance()
        .unwrap();
    assert_eq!(
        frozen.entities, 0,
        "delivery owns frozen data before a mutable family exists"
    );
    assert_eq!(
        frozen.root_and_control_bytes, 146,
        "one frame plus a frozen root and 66-byte list"
    );
    incoming.reopen(crate::Layout::default(), false);
    assert_eq!(
        incoming
            .machine
            .heap
            .logical_ledger()
            .borrow()
            .balance()
            .unwrap(),
        frozen
    );
    assert!(matches!(incoming.run_to_end(&[]), crate::End::Finished(_)));
    incoming.machine.logical_flush().unwrap();
    let family = incoming
        .machine
        .heap
        .logical_ledger()
        .borrow()
        .balance()
        .unwrap();
    assert_eq!(family.entities, 1);
    assert_eq!(
        family.entity_local_bytes, 66,
        "instantiation transfers the complete frozen price to its cold family"
    );
    assert_eq!(family.root_and_control_bytes, 16);
}
