use super::support::*;

#[derive(Clone, Copy, Debug)]
struct RlmProtocolScenarioCoverage {
    display_name: &'static str,
}

const NATURAL_CELL_MAX_TURN: RlmProtocolScenarioCoverage = RlmProtocolScenarioCoverage {
    display_name: "natural cell max-turn stop",
};

#[test]
fn rlm_protocol_scenario_natural_cell_at_budget_stops_without_another_provider_call() {
    RlmProtocolScenario::new(NATURAL_CELL_MAX_TURN.display_name)
        .user_message("run exactly one cell")
        .max_turns(1)
        .llm_response(vec![typescript_block("console.log(\"allowed\")")])
        .exec_result(exec_response(&["allowed"], None, None))
        .expect(RlmProtocolExpectations {
            exec_codes: vec!["console.log(\"allowed\")"],
            llm_call_count: Some(1),
            done: Some(true),
            transcript_system_message_count: Some(0),
            turn_outcome: Some(lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::MaxTurns,
            )),
            ..RlmProtocolExpectations::default()
        })
        .run();
}
