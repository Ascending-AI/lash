use super::*;

pub(super) fn reserved_invocations() {
    for first in [51, u64::MAX - 3] {
        for (charge, depth, live) in [
            (1, u32::MAX, 0usize),
            (2, u32::MAX, 1),
            (7, u32::MAX, 2),
            (u64::MAX, 2, 2),
            (u64::MAX, u32::MAX, 0),
        ] {
            let mut state = State::new();
            let view = state.view();
            let mut lease = RegionLease::acquire_fixture_reserved(
                &mut state,
                recursive(2),
                Limits {
                    charge,
                    depth,
                    ..limits()
                },
                first,
                3,
            )
            .expect("an actual reservation may follow already completed task calls");
            let stop = lease
                .run(StopAt::NextBoundary)
                .expect("reserved invocation execution");
            let prefix = lease.materialize().expect("current reserved activations");
            let image = prefix.reconstruction();
            assert_eq!(
                image.issued_invocations,
                if charge == u64::MAX && depth == u32::MAX {
                    3
                } else {
                    live as u64
                }
            );
            let mut expected = vec![1];
            expected.extend((0..live).map(|offset| first + offset as u64));
            assert_eq!(
                image
                    .frames
                    .iter()
                    .map(|frame| frame.invocation)
                    .collect::<Vec<_>>(),
                expected
            );
            if charge == u64::MAX && depth == u32::MAX {
                assert_eq!(stop, RegionStop::Complete);
                assert!(
                    image
                        .payload
                        .expect("actual recursive result")
                        .same(&Value::Int(2))
                );
            } else {
                assert_eq!(stop, RegionStop::Refusal);
            }
            view.inspect(|image, revision, transactions| {
                assert!(image.is_none());
                assert_eq!((revision, transactions), (0, 0));
            });
            prefix
                .prepare_exit()
                .expect("one reservation exit")
                .commit()
                .expect("one canonical swap");
        }
    }
}
