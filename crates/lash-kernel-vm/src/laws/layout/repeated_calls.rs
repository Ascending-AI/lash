//! K-EVAL-008 / K-EFF-008: helper calls share the loop publication.
use super::*;
use crate::machine::islands::witness::Counts;

pub(super) fn runtime(collect: &impl Fn(Counts)) {
    // K-EVAL-008 / K-EFF-008: actual helper bodies keep their logical
    // invocations and action occurrences inside one loop publication.
    let helper = super::super::embedder::fragment(
        "function advance(a: Any, b: Any) -> Any\nkernel 1\nmetering statements\nbody { if flat.lt(a, b) { return flat.add(a, b) } return flat.add(a, b) }",
    );
    for (body, expected_sum) in [
        (
            "let i = 0 let s = 0 while flat.lt(i, 4) { set s = flat.add(s, flat.mul(i, i)) set i = invoke advance(i, 1) } return (s, i)",
            14,
        ),
        (
            "let i = 0 let s = 0 while flat.lt(i, 4) { set s = flat.add(s, invoke advance(i, i)) set i = flat.add(i, 1) } return (s, i)",
            12,
        ),
    ] {
        let composed = format!("definition \"{helper}\"\n{}", scalar_document(body));
        crate::machine::islands::REPEATED_TEMPLATE.with(|n| n.set(0));
        crate::machine::islands::compiled::take_loop_publications();
        let roomy = observe_scalar(&composed, Layout(0), false, false, false);
        assert_eq!(
            roomy.0,
            format!(
                "finished {:?}",
                Datum::Tuple(vec![int(expected_sum), int(4)])
            )
        );
        assert_eq!(
            crate::machine::islands::compiled::take_loop_publications(),
            1,
            "a guarded helper loop must publish once"
        );
        assert_eq!(
            crate::machine::islands::REPEATED_TEMPLATE.with(|n| n.get()),
            4,
            "four real composed iterations"
        );
        for record in [false, true] {
            collect(packet_modes(
                &composed,
                super::super::embedder::ROOMY,
                u64::MAX,
                false,
                record,
            ));
            for slice in [1, 3, 11] {
                collect(packet_modes(
                    &composed,
                    super::super::embedder::ROOMY,
                    slice,
                    true,
                    record,
                ));
            }
        }
        for charge_limit in 0..roomy.4 {
            collect(packet_modes(
                &composed,
                Bounds {
                    charge_limit,
                    ..super::super::embedder::ROOMY
                },
                u64::MAX,
                false,
                true,
            ));
        }
        for memory in [0, 64, 128, 256, 512, 1024, 2048, 4096] {
            collect(packet_modes(
                &composed,
                Bounds {
                    memory,
                    ..super::super::embedder::ROOMY
                },
                u64::MAX,
                false,
                true,
            ));
        }
        for call_depth in [0, 1, 2, 3] {
            collect(packet_modes(
                &composed,
                Bounds {
                    call_depth,
                    ..super::super::embedder::ROOMY
                },
                u64::MAX,
                false,
                true,
            ));
        }
        crate::machine::islands::compiled::take_loop_publications();
        collect(packet_modes(
            &composed,
            super::super::embedder::ROOMY,
            roomy.4 / 2,
            true,
            true,
        ));
        assert!(
            crate::machine::islands::compiled::take_loop_publications() > 0,
            "cold reopening must follow a real composed loop publication"
        );
        let refused = crate::machine::islands::compiled::with_substituted_loop_result(|| {
            packet_modes(
                &composed,
                super::super::embedder::ROOMY,
                u64::MAX,
                false,
                true,
            )
        });
        assert_eq!(
            crate::machine::islands::compiled::take_loop_publications(),
            0,
            "a substituted kernel answer cannot publish"
        );
        collect(refused);
        eprintln!(
            "D guarded helper runtime matrix completed; 4 iterations / 1 publication in the roomy run"
        );
    }
}
