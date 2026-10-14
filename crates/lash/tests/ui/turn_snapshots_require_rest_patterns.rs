fn output(output: lash::TurnOutput) {
    let lash::TurnOutput { result, activities } = output;
    let lash::TurnReport {
        outcome,
        usage,
        llm_calls,
        failure_evidence,
        tool_calls,
        execution,
        errors,
        acceptance,
        cancel_input_outcome,
        source,
    } = result;
}

fn main() {}
