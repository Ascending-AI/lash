use crate::*;

#[test]
fn the_semantics_cover_every_op_and_dynamic_charge() {
    assert_eq!(verify_semantics(SEMANTICS), Ok(()));
}

#[test]
fn the_coverage_verifier_rejects_a_missing_case() {
    let without = |op: OpKind| -> Vec<OpSpec> {
        SEMANTICS
            .iter()
            .copied()
            .filter(|spec| spec.op != op)
            .collect()
    };
    assert_eq!(
        verify_semantics(&without(OpKind::Caught)),
        Err(CoverageError::MissingOp(OpKind::Caught))
    );

    let mut swapped = SEMANTICS.to_vec();
    swapped.swap(0, 1);
    assert_eq!(
        verify_semantics(&swapped),
        Err(CoverageError::Misplaced {
            at: 0,
            op: OpKind::Stmt
        })
    );

    let mut unemitted = SEMANTICS.to_vec();
    for spec in &mut unemitted {
        if spec.op == OpKind::CallNative {
            spec.events = &[];
        }
    }
    assert_eq!(
        verify_semantics(&unemitted),
        Err(CoverageError::UnemittedEvent(ChargeEvent::NativeFormula))
    );
}

#[test]
fn every_fixture_verifies() {
    for fixture in fixtures::all() {
        assert_eq!(
            verify_function(&fixture.function),
            Ok(()),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn a_cycle_through_no_boundary_is_rejected() {
    let fixture = fixtures::pick_field();
    let mut f = fixture.function;
    // A block that charges and jumps back to itself: a loop no slice test
    // would ever reach.
    let start = f.ops.len() as u32;
    let mut ops = f.ops.to_vec();
    ops.push(Op::Charge {
        units: 1,
        event: ChargeEvent::ExprNode,
    });
    ops.push(Op::Jump { to: KBlockIx(1) });
    let mut origins = f.origins.to_vec();
    origins.extend([origins[0].clone(), origins[0].clone()]);
    let mut blocks = f.blocks.to_vec();
    blocks.push(KBlock {
        ops: OpRange { start, len: 2 },
        exception: None,
    });
    f.ops = ops.into();
    f.origins = origins.into();
    f.blocks = blocks.into();
    assert_eq!(
        verify_function(&f),
        Err(KirError::UncoveredCycle(KBlockIx(1)))
    );
}
