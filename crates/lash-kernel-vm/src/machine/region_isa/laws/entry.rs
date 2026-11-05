use super::*;

/// K-EVAL-008: a resumed region binds the actual already-reached occurrence;
/// acquiring it must not repeat that prefix's counter edits or entry values.
pub(super) fn resumed_entries() {
    let code = read_loop(17);
    let programs = super::super::programs::Programs::bind(Box::new([Arc::clone(&code)])).unwrap();
    for (invocation, done) in [(51, 1), (73, 3)] {
        let entry = super::super::entry::Entry {
            program: code.id,
            invocation,
            pc: 0,
            prefix: 5,
            occurrence: Some(17 + done - 1),
            values: vec![
                (Register::Int(0), Value::Int(done as i64)),
                (Register::Int(1), Value::Int(17)),
                (Register::Int(2), Value::Int(1)),
                (Register::Int(3), Value::Int(7 * done as i64)),
                (Register::Value(0), object(99)),
            ]
            .into_boxed_slice(),
            source_counts: Box::new([17 + done, 91 + done]),
        };
        let view = super::super::storage_fixture::FixtureView::bind(&programs, &code).unwrap();
        let mut execution = Execution::acquire_programs(
            Arc::clone(&programs),
            entry,
            Limits {
                entry_charge: 6 * done,
                entry_progress: 6 * done,
                ..limits()
            },
            super::super::invocations::Invocations::fixture(invocation + 1, 1).unwrap(),
            view,
        )
        .unwrap();
        assert_eq!(execution.reached.occurrence, Some(17 + done - 1));
        assert_eq!(execution.source.current(0).unwrap(), 17 + done);
        assert_eq!(execution.source.current(1).unwrap(), 91 + done);
        assert_eq!(
            execution.run(StopAt::NextBoundary).unwrap(),
            RegionStop::Complete
        );
        let image = execution.reconstruct().unwrap().image;
        assert_eq!(
            (image.prefix.invocation, image.counters.charge),
            (invocation, 102)
        );
        assert_eq!(&*image.source_counts, [34, 108]);
        assert_eq!(image.prefix.occurrence, Some(33));
        assert!(image.payload.unwrap().same(&Value::Int(119)));
        assert!(role(&image, 0, 0).same(&Value::Int(17)));
        assert!(role(&image, 0, 1).same(&Value::Int(119)));
    }
}
