use super::*;
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
}
