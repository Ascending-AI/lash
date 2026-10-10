use std::collections::BTreeMap;

use lash_kernel_doc::{DocumentId, FunctionId, Site, Unit};

use crate::*;

/// The pinned [`EXEC_ABI_DIGEST`]. A change to the op list, the runtime
/// table, a layout or the map encoding changes it; re-pin it in the same
/// change, which is what retires every artifact built against the old ABI.
const PINNED_ABI_DIGEST: &str = "cc285e08484a7b0c6a22b6831048d96953f7bb46111d1bf93f99c37a99a11011";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn the_abi_digest_is_pinned() {
    assert_eq!(
        hex(&*EXEC_ABI_DIGEST),
        PINNED_ABI_DIGEST,
        "the ABI changed; its description is:\n{}",
        abi_description()
    );
}

#[test]
fn the_semantics_cover_every_op_runtime_function_and_dynamic_charge() {
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

    let mut unreached = SEMANTICS.to_vec();
    for spec in &mut unreached {
        if spec.op == OpKind::Clock {
            spec.calls = &[];
        }
    }
    assert_eq!(
        verify_semantics(&unreached),
        Err(CoverageError::UnreachedRtFn(RtFn::Clock))
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
fn every_fixture_verifies_with_one_map_per_boundary() {
    for fixture in fixtures::all() {
        assert_eq!(
            verify_function(&fixture.function, &fixture.maps),
            Ok(()),
            "{}",
            fixture.name
        );
        assert_eq!(fixture.maps.len(), fixture.function.boundaries.len());
    }
}

#[test]
fn a_saveable_exit_without_a_map_is_rejected() {
    let fixture = fixtures::call_helper();
    let mut maps = fixture.maps.clone();
    // The call-return Boundary: the exit an `Act` takes.
    maps.remove(1);
    assert_eq!(
        verify_function(&fixture.function, &maps),
        Err(KirError::MissingMap(BoundaryIx(1)))
    );
}

#[test]
fn a_cycle_through_no_boundary_is_rejected() {
    let fixture = fixtures::pick_field();
    let mut f = fixture.function;
    // A block that charges and jumps back to itself: a loop no slice test
    // and no watchdog would ever reach.
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
        verify_function(&f, &fixture.maps),
        Err(KirError::UncoveredCycle(KBlockIx(1)))
    );
}

#[test]
fn frame_maps_decode_to_what_was_encoded() {
    let mut maps = fixtures::guarded().maps;
    maps[5] = FrameMap {
        boundary: BoundaryIx(5),
        slots: Box::new([(SlotAt(1), Loc::Reg(Reg(3))), (SlotAt(0), Loc::Empty)]),
        loops: Box::new([]),
        finally: Box::new([(TryIx(0), DepartLoc::Throw(Loc::Spill(300)))]),
        charge_prefix: u64::MAX,
        memory_prefix: 1 << 40,
    };
    let bytes = encode_maps(&maps);
    assert_eq!(decode_maps(&bytes), Ok(maps));
    assert_eq!(
        decode_maps(&bytes[..bytes.len() - 1]),
        Err(MapDecodeError::Truncated)
    );
}

#[test]
fn an_artifact_key_binds_every_input() {
    let base = KeyInput {
        subject: Subject::Library(FunctionId::from_bytes([1; 32])),
        deps: BTreeMap::from([(FunctionId::from_bytes([2; 32]), LibRunKind::Body)]),
        kernel: 1,
        exec_abi: *EXEC_ABI_DIGEST,
        codegen: [3; 32],
        pe: None,
        target: TargetId {
            triple: "x86_64-unknown-linux-gnu".into(),
            cpu: "x86-64-v2".into(),
            page_size: 4096,
            endian: Endian::Little,
        },
        flags: CodeFlags {
            opt: OptLevel::Baseline,
            maps: MapDetail::Exits,
            watchdog: true,
        },
    };
    type Change = Box<dyn Fn(&mut KeyInput)>;
    let variants: Vec<Change> = vec![
        Box::new(|k| {
            k.subject =
                Subject::Residual(FunctionId::from_bytes([1; 32]), VariantFactDigest([0; 32]));
        }),
        Box::new(|k| {
            k.subject = Subject::Document(
                DocumentId::from_bytes([1; 32]),
                Site::new(Unit::Main, vec![0]),
            );
        }),
        Box::new(|k| {
            k.deps.insert(
                FunctionId::from_bytes([2; 32]),
                LibRunKind::Native { probe: [0; 32] },
            );
        }),
        Box::new(|k| k.kernel = 2),
        Box::new(|k| k.exec_abi = [0; 32]),
        Box::new(|k| k.codegen = [4; 32]),
        Box::new(|k| k.pe = Some([0; 32])),
        Box::new(|k| k.target.triple = "aarch64-unknown-linux-gnu".into()),
        Box::new(|k| k.target.cpu = "x86-64-v3".into()),
        Box::new(|k| k.target.page_size = 16384),
        Box::new(|k| k.target.endian = Endian::Big),
        Box::new(|k| k.flags.opt = OptLevel::Optimized),
        Box::new(|k| k.flags.maps = MapDetail::Origins),
        Box::new(|k| k.flags.watchdog = false),
    ];
    let mut keys = vec![base.key()];
    for change in &variants {
        let mut input = base.clone();
        change(&mut input);
        keys.push(input.key());
    }
    let distinct: std::collections::BTreeSet<_> = keys.iter().collect();
    assert_eq!(distinct.len(), keys.len());
    // A site's path and a function name are length-prefixed: these two
    // document subjects encode differently.
    let site = |path: Vec<u32>| {
        let mut input = base.clone();
        input.subject = Subject::Document(
            DocumentId::from_bytes([1; 32]),
            Site::new(Unit::Function(lash_kernel_doc::Name::new("f")), path),
        );
        input.key()
    };
    assert_ne!(site(vec![1, 2]), site(vec![1]));
}
