use super::*;

/// A completed construction never becomes its own unpaid head. Navigation
/// follows source evaluation order and declines a future cursor decision.
pub(super) fn successors() {
    Embedder::new("main { return null }")
        .machine
        .check_prepared_constructor_graphs();
    prepared_literals_and_captures();
    use crate::compile::room::{self, Phase, Point};
    use lash_kernel_value::sites::{Site, Unit};
    for (text, completed, next, phase) in [
        (
            "main { let x = 1 set x = 2 return x }",
            vec![1],
            vec![2, 0],
            Phase::Commit,
        ),
        (
            "main { let x = 1 if true { set x = 2 } return x }",
            vec![1, 1, 0],
            vec![1, 1],
            Phase::Leave,
        ),
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
    let suffix = source.compile_departure_suffix(&returning).unwrap();
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
            .compile_departure_suffix(&Point {
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

fn prepared_literals_and_captures() {
    use crate::compile::CodeId;

    use lash_kernel_doc as doc;

    use std::sync::Arc;
    let run = Embedder::new(
        "main { let make = fn(x) { let f = fn() { return x } return f } return apply make(7) }",
    );
    let (program, _) = run.program(false);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("closure source admits"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("closure source compiles"));
    let (code_id, target) = (0..exe.code_count())
        .map(|index| (CodeId(index as u32), exe.code(CodeId(index as u32))))
        .find(|(_, code)| code.created.is_some() && code.captures.len() == 1)
        .unwrap();
    let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
        panic!("closure has a source function");
    };
    let identity = Arc::new(doc::arena::ClosureIdentity {
        capture_names: target.capture_names.clone(),
        callable: lash_kernel_value::names::CallableId {
            link: program.link,
            function,
        },
        site: doc::linked_site::LinkedSite {
            link: program.link,
            site: target.created.as_ref().unwrap().clone(),
        },
    });
    let code = doc::arena::LocalCode {
        executable: exe.local(),
        code: code_id.0,
    };
    run.machine
        .check_prepared_constructor_literals(identity, code);
}
