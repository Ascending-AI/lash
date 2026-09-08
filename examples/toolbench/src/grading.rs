use serde::Serialize;
use serde_json::Value;

use crate::tasks::Task;
use crate::world::World;

#[derive(Clone, Debug, Default)]
pub(crate) struct RunEvidence {
    pub(crate) standard: bool,
    pub(crate) rounds: usize,
    pub(crate) submit_count: usize,
    pub(crate) attempts: Vec<serde_json::Value>,
    pub(crate) wall_ms: u128,
    pub(crate) completed: bool,
    pub(crate) completion_error: Option<String>,
    pub(crate) finish_value: Option<Value>,
    pub(crate) iterations: usize,
    pub(crate) executions: usize,
    pub(crate) tool_call_count: usize,
    pub(crate) failed_execution_errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Grade {
    pub(crate) passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure_reason: Option<String>,
}

pub(crate) fn grade(
    task: &Task,
    final_world: &World,
    evidence: &RunEvidence,
    max_task_cost_usd: f64,
) -> Grade {
    let mut failures = Vec::new();

    if evidence.completion_error.as_deref() == Some("wall_limit") {
        return Grade {
            passed: false,
            failure_reason: Some("wall_limit".into()),
        };
    }
    if !evidence.completed {
        failures.push(format!(
            "turn did not complete{}",
            evidence
                .completion_error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        ));
    }
    if final_world != &task.expected_world {
        failures.push("mock-world end state differs from the exact expected state".to_string());
    }
    if !task.finish.matches(evidence.finish_value.as_ref()) {
        failures.push(format!(
            "finish mismatch: expected {}; got {}",
            task.finish.describe(),
            evidence
                .finish_value
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "<none>".to_string())
        ));
    }
    if crate::summary::Usage::from_attempts(&evidence.attempts)
        .cost
        .is_some_and(|cost| cost > max_task_cost_usd)
    {
        failures.push("cost_limit".to_string());
    }
    if evidence.standard {
        if evidence.submit_count > 1 {
            failures.push("repeated submit".to_string());
        } else if evidence.submit_count == 0 {
            failures.push("missing submit".to_string());
        }
    }

    Grade {
        passed: failures.is_empty(),
        failure_reason: (!failures.is_empty()).then(|| failures.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tasks::{FinishMatcher, Task};

    fn fixture() -> Task {
        let seed = World::seeded();
        let mut expected_world = seed.clone();
        expected_world
            .kv
            .insert("status".to_string(), "ready".to_string());
        Task {
            id: "grader-fixture",
            prompt: "fixture".into(),
            seed,
            expected_world,
            finish: FinishMatcher::Exact(json!("saved")),
            tool_calls: 1,
        }
    }

    fn passing_evidence() -> RunEvidence {
        RunEvidence {
            completed: true,
            finish_value: Some(json!("saved")),
            iterations: 1,
            tool_call_count: 1,
            ..RunEvidence::default()
        }
    }

    #[test]
    fn correctness_accepts_extra_work_and_recovery() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.executions = 7;
        evidence.failed_execution_errors = vec!["same error".into(); 4];
        evidence.tool_call_count = 9;
        evidence.rounds = 12;
        for standard in [false, true] {
            evidence.standard = standard;
            evidence.submit_count = 1;
            assert!(grade(&task, &task.expected_world, &evidence, 0.10).passed);
        }
    }

    #[test]
    fn cost_ceiling_sums_calls_accepts_boundary_and_exposes_missing_cost() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.attempts = vec![json!({"cost":0.04}), json!({"cost":0.06})];
        assert!(grade(&task, &task.expected_world, &evidence, 0.10).passed);
        assert_eq!(
            grade(&task, &task.expected_world, &evidence, 0.09)
                .failure_reason
                .as_deref(),
            Some("cost_limit")
        );
        evidence.attempts.push(json!({"cost":null}));
        assert!(grade(&task, &task.expected_world, &evidence, 0.09).passed);
        assert_eq!(
            crate::summary::Usage::from_attempts(&evidence.attempts).cost,
            None
        );
    }

    #[test]
    fn correctness_and_completion_remain_required() {
        let task = fixture();
        let mut evidence = passing_evidence();
        assert!(grade(&task, &task.expected_world, &evidence, 0.10).passed);
        assert!(!grade(&task, &task.seed, &evidence, 0.10).passed);
        evidence.finish_value = Some(json!("wrong"));
        assert!(!grade(&task, &task.expected_world, &evidence, 0.10).passed);
        evidence.finish_value = None;
        assert!(!grade(&task, &task.expected_world, &evidence, 0.10).passed);
        evidence = passing_evidence();
        evidence.completed = false;
        assert!(!grade(&task, &task.expected_world, &evidence, 0.10).passed);
        evidence.completion_error = Some("wall_limit".into());
        assert_eq!(
            grade(&task, &task.expected_world, &evidence, 0.10)
                .failure_reason
                .as_deref(),
            Some("wall_limit")
        );
    }

    #[test]
    fn standard_requires_exactly_one_submit() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.standard = true;
        for count in [0, 2] {
            evidence.submit_count = count;
            assert!(!grade(&task, &task.expected_world, &evidence, 0.10).passed);
        }
        evidence.submit_count = 1;
        assert!(grade(&task, &task.expected_world, &evidence, 0.10).passed);
    }
}
