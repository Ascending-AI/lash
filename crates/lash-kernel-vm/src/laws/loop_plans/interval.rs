use crate::compile::room::{
    Balance, CallSource, CheckedRoom, Execution, Facts, Measure, Owner, Phase, Point, Refused,
    Selection, Source,
};
use lash_kernel_value::{
    FunctionId,
    sites::{Site, Unit},
};

struct Trace {
    tuple: Site,
    points: Vec<Point>,
}
impl Execution for Trace {
    fn event(&mut self, at: &Point) -> Option<Box<dyn Facts + '_>> {
        self.points.push(at.clone());
        Some(Box::new(Values(self.tuple.clone())))
    }
    fn choose(&mut self, _: &Site) -> Option<bool> {
        None
    }
}
struct Values(Site);
impl Values {
    fn local(&self, site: &Site) -> i128 {
        // The tuple owns its 16-byte header and two ordinary 16-byte values.
        if site == &self.0 { 48 } else { 16 }
    }
}
impl Facts for Values {
    fn measure(&self, measure: &Measure) -> Option<i128> {
        match measure {
            Measure::Local(Owner::Operand(at)) => Some(self.local(at)),
            Measure::Release(owners) => owners.iter().try_fold(0, |sum, owner| match owner {
                Owner::Operand(at) => Some(sum + self.local(at)),
                _ => None,
            }),
            _ => None,
        }
    }
    fn call(&self, _: &Site, _: CallSource) -> Option<&CheckedRoom> {
        None
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(&self, _: &Site) -> Option<FunctionId> {
        None
    }
}

/// A committed member read stays an input to the suffix. Compiling that suffix
/// must neither replay the read nor require the enclosing expression to finish.
pub(super) fn member_read_suffix() {
    let document = lash_kernel_doc::parse_document(
        "numbers by_spelling\nkernel 1\nmain { let xs = [null] return (xs[0], true) }",
    )
    .unwrap();
    let root = Site::new(Unit::Main, vec![]);
    let source = Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(document.identity()),
        root.clone(),
        document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &Default::default(),
    );
    let tuple = root.child(1).child(0);
    let mut whole = Trace {
        tuple: tuple.clone(),
        points: vec![],
    };
    let oracle = source
        .evaluate_prefix(&tuple, &mut whole, 32)
        .unwrap()
        .balance();
    let mut prefix = Trace {
        tuple: tuple.clone(),
        points: vec![],
    };
    let before = source
        .evaluate_prefix(&tuple.child(0), &mut prefix, 32)
        .unwrap()
        .balance();
    assert_eq!(
        oracle,
        Balance {
            peak_extra: 48,
            retained_delta: 48
        }
    );
    assert_eq!(
        before,
        Balance {
            peak_extra: 48,
            retained_delta: 16
        }
    );
    let route: Vec<_> = whole
        .points
        .iter()
        .map(|at| Selection::Event {
            at: at.clone(),
            exit: crate::compile::room::ExitKind::Normal,
        })
        .collect();
    let from = Point {
        site: tuple.child(1),
        phase: Phase::Commit,
    };
    let through = Point {
        site: tuple.clone(),
        phase: Phase::Commit,
    };
    let suffix = source.compile_between(&from, &through, &route, 32).unwrap();
    let mut execution = Trace {
        tuple: tuple.clone(),
        points: vec![],
    };
    let after = suffix.evaluate_interval(&mut execution).unwrap().balance();
    assert_eq!(
        after,
        Balance {
            peak_extra: 32,
            retained_delta: 32
        }
    );
    assert_eq!(execution.points, [from.clone(), through.clone()]);
    assert_eq!(
        oracle.retained_delta,
        before.retained_delta + after.retained_delta
    );
    assert_eq!(
        u128::from(oracle.peak_extra),
        u128::from(before.peak_extra)
            .max(u128::try_from(before.retained_delta).unwrap() + u128::from(after.peak_extra))
    );
    // Before this expression, xs owns one 48-byte list, a 16-byte binding and
    // a 64-byte frame. Check every finite bound through the first spare byte.
    let held = 128_u64;
    for bound in 0..=held + oracle.peak_extra + 1 {
        let whole_fits = held + oracle.peak_extra <= bound;
        let split_fits = held + before.peak_extra <= bound
            && u128::from(held)
                + u128::try_from(before.retained_delta).unwrap()
                + u128::from(after.peak_extra)
                <= u128::from(bound);
        assert_eq!(whole_fits, split_fits, "finite bound {bound}");
    }
    let read = prefix.points.last().unwrap();
    let middle = source
        .compile_between(read, &from, &route[..route.len() - 1], 32)
        .unwrap();
    let mut execution = Trace {
        tuple,
        points: vec![],
    };
    middle.evaluate_interval(&mut execution).unwrap();
    assert_eq!(execution.points, [read.clone(), from]);
    let mut fake = route.clone();
    fake.remove(0);
    assert!(matches!(
        source.compile_between(&whole.points[3], &through, &fake, 32),
        Err(Refused::Path | Refused::Exit)
    ));
}
