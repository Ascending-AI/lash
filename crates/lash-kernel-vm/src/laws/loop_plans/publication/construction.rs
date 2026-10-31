use super::*;

/// A completed construction never becomes its own unpaid head. Navigation
/// follows source evaluation order and declines a future cursor decision.
pub(super) fn successors() {
    use crate::compile::room::{self, Phase, Point};
    use lash_kernel_value::sites::{Site, Unit};
    for (text, completed, next, phase) in [
        (
            "main { let xs = [1, 2] return xs }",
            vec![0],
            vec![1, 0],
            Phase::Commit,
        ),
        (
            "main { if true { let xs = [1] } return 1 }",
            vec![0, 1, 0],
            vec![0, 1],
            Phase::Leave,
        ),
        (
            "main { let xs = [1, 2] return xs }",
            vec![0, 0],
            vec![0],
            Phase::Commit,
        ),
        (
            "main { return [[1], [2]] }",
            vec![0, 0, 0],
            vec![0, 0, 1, 0],
            Phase::Commit,
        ),
        (
            "main { return [[1], [2]] }",
            vec![0, 0, 1],
            vec![0, 0],
            Phase::Commit,
        ),
        (
            "main { return if(true, [1], [2]) }",
            vec![0, 0, 1],
            vec![0, 0],
            Phase::Commit,
        ),
        (
            "main { return if(false, [1], [2]) }",
            vec![0, 0, 2],
            vec![0, 0],
            Phase::Commit,
        ),
        (
            "main { return if([], 1, 2) }",
            vec![0, 0, 0],
            vec![0, 0],
            Phase::Test,
        ),
        (
            "main { return let(x = [1], x) }",
            vec![0, 0, 0],
            vec![0, 0],
            Phase::Test,
        ),
    ] {
        let run = Embedder::new(text);
        let (program, _) = run.program(false);
        let source = room::Source::compile(
            lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
            Site::new(Unit::Main, vec![]),
            program.document.main.as_ref().unwrap(),
            &[],
            Default::default(),
            &std::collections::BTreeMap::new(),
            &std::collections::BTreeSet::new(),
        );
        let at = Site::new(Unit::Main, completed);
        let expected = Point {
            site: Site::new(Unit::Main, next),
            phase,
        };
        assert_eq!(source.next_after_commit(&at).unwrap(), expected, "{text}");
        assert_eq!(
            source.next_after_commit(&at).unwrap(),
            expected,
            "cached {text}"
        );
    }
    let run = Embedder::new("main { let xs = [1] for x in xs { print x } return xs }");
    let (program, _) = run.program(false);
    let source = room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    );
    let returning = Point {
        site: Site::new(Unit::Main, vec![2]),
        phase: Phase::Commit,
    };
    let suffix = source.compile_return_suffix(&returning).unwrap();
    assert_eq!(
        suffix
            .events()
            .map(|event| event.point.clone())
            .collect::<Vec<_>>(),
        vec![
            returning,
            Point {
                site: source.site.clone(),
                phase: Phase::Leave
            }
        ]
    );
    assert_eq!(suffix.exit(), room::ExitKind::Return);
    assert!(
        source
            .compile_return_suffix(&Point {
                site: Site::new(Unit::Main, vec![0]),
                phase: Phase::Commit
            })
            .is_err()
    );
    let run = Embedder::new("main { for x in [] {} return null }");
    let (program, _) = run.program(false);
    let source = room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    );
    assert!(
        source
            .next_after_commit(&Site::new(Unit::Main, vec![0, 0]))
            .is_err(),
        "a future cursor decision needs its actual runtime fact"
    );
    assert!(
        source
            .next_after_commit(&Site::new(Unit::Main, vec![0]))
            .is_err(),
        "an expression cursor cannot navigate a statement Commit"
    );
}
