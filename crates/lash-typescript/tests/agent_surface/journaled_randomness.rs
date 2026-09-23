use super::*;

/// Every `Math.random()` draw crosses the journal, in order, so a replayed turn
/// reproduces the sequence it drew the first time.
///
/// This is the one accepted operation with no oracle: pinning it against Node
/// is impossible by construction. The property that makes it admissible in a
/// durable program is not the distribution but the seam — the VM samples no
/// RNG of its own, so a host serving a recorded journal replays the run
/// exactly. If a draw were ever computed in-VM, the second run below would
/// still succeed while the host's draw count fell short.
#[test]
fn math_random_draws_replay_from_the_journal_in_order() {
    struct JournalHost {
        recorded: Vec<f64>,
        served: std::sync::Mutex<usize>,
    }

    impl ExecutionHost for JournalHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => {
                    assert_eq!(
                        operation.operation.as_str(),
                        lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION
                    );
                    let mut cursor = self.served.lock().expect("journal cursor");
                    let value = *self
                        .recorded
                        .get(*cursor)
                        .expect("the journal has a recorded draw for every call");
                    *cursor += 1;
                    Ok(AbilityResult::Value(Value::Number(value)))
                }
                AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
                _ => Err(ExecutionHostError::new("unexpected ability")),
            }
        }
    }

    let recorded = vec![0.125, 0.5, 0.875, 0.0];
    let program = lash_typescript::testing::compile(
        "const out: number[] = []; for (let i = 0; i < 4; i++) { out[out.length] = Math.random(); } finish(out.join(','));",
    )
    .expect("a journaled random sequence should compile");

    let mut results = Vec::new();
    for _ in 0..2 {
        let host = JournalHost {
            recorded: recorded.clone(),
            served: std::sync::Mutex::new(0),
        };
        let outcome =
            futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
                .expect("a journaled random sequence should execute");
        assert_eq!(
            *host.served.lock().expect("journal cursor"),
            recorded.len(),
            "every draw must reach the host"
        );
        results.push(outcome);
    }
    assert_eq!(results[0], results[1], "replay must reproduce the sequence");
    assert_eq!(
        results[0],
        ExecutionOutcome::Finished(Value::String("0.125,0.5,0.875,0".into()))
    );
}
