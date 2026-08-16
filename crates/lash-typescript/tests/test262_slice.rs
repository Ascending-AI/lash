use std::path::Path;

use lashlang::{AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected test262 ability")),
        }
    }
}

#[test]
fn curated_test262_slice_passes_through_the_real_pipeline() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test262/fixtures");
    let mut paths = std::fs::read_dir(fixtures)
        .expect("read test262 fixtures")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "ts"))
        .collect::<Vec<_>>();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "the curated test262 slice must not be empty"
    );

    for path in paths {
        let source = std::fs::read_to_string(&path).expect("read test262 fixture");
        let program = lash_typescript::compile(&source)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let outcome = futures::executor::block_on(lashlang::execute(
            &program,
            &mut lashlang::State::new(),
            &Host,
        ))
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(lashlang::Value::Bool(true)),
            "{}",
            path.display()
        );
    }
}
