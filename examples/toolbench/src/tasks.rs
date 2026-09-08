use serde::Serialize;
use serde_json::{Value, json};

use crate::world::World;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "expected")]
pub(crate) enum FinishMatcher {
    Exact(Value),
    Normalized(String),
}

impl FinishMatcher {
    pub(crate) fn matches(&self, actual: Option<&Value>) -> bool {
        match (self, actual) {
            (Self::Exact(expected), Some(actual)) => expected == actual,
            (Self::Normalized(expected), Some(Value::String(actual))) => {
                normalize(expected) == normalize(actual)
            }
            _ => false,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Exact(value) => format!("finish exactly {value}"),
            Self::Normalized(value) => format!("normalized finish equals {value:?}"),
        }
    }
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Clone, Debug)]
pub(crate) struct Task {
    pub(crate) id: &'static str,
    pub(crate) prompt: &'static str,
    pub(crate) seed: World,
    pub(crate) expected_world: World,
    pub(crate) finish: FinishMatcher,
    pub(crate) tool_calls: usize,
}

impl Task {
    pub(crate) fn checker_description(&self) -> String {
        format!(
            "{}; exact seeded-world equality; exactly {} tool call(s); turn completes; at most 2 code executions; at most 2 failed executions; no repeated identical execution error",
            self.finish.describe(),
            self.tool_calls
        )
    }
}

pub(crate) fn task_pack() -> Vec<Task> {
    vec![
        read_task(
            "weather-temperature",
            "Call weather.lookup for Berlin, then finish with the plain string 12 (the temperature in Celsius), not a number; do not JSON-encode or wrap the string. Do not call any other tool.",
            FinishMatcher::Exact(json!("12")),
            1,
        ),
        read_task(
            "weather-condition",
            "Call weather.lookup for Berlin, then finish with its condition as a plain string; do not JSON-encode or wrap the string. Do not call any other tool.",
            FinishMatcher::Normalized("rain".to_string()),
            1,
        ),
        read_task(
            "kv-read",
            "Call kv.get for project and finish with only its value as a plain string; do not JSON-encode or wrap the string; call no other tools.",
            FinishMatcher::Exact(json!("aurora")),
            1,
        ),
        read_task(
            "mail-count",
            "Call mail.list once and finish with the plain string 2, the number of messages, not a number; do not JSON-encode or wrap the string; call no other tools.",
            FinishMatcher::Exact(json!("2")),
            1,
        ),
        write_task(
            "weather-to-kv",
            "Call weather.lookup for Lisbon, then call kv.put to store its condition under key last_weather. Finish with only the stored condition as a plain string; do not JSON-encode or wrap the string; change nothing else.",
            FinishMatcher::Exact(json!("sunny")),
            2,
            |world| {
                world
                    .kv
                    .insert("last_weather".to_string(), "sunny".to_string());
            },
        ),
    ]
}

fn read_task(
    id: &'static str,
    prompt: &'static str,
    finish: FinishMatcher,
    tool_calls: usize,
) -> Task {
    write_task(id, prompt, finish, tool_calls, |_| {})
}

fn write_task(
    id: &'static str,
    prompt: &'static str,
    finish: FinishMatcher,
    tool_calls: usize,
    mutate_expected: impl FnOnce(&mut World),
) -> Task {
    let seed = World::seeded();
    let mut expected_world = seed.clone();
    mutate_expected(&mut expected_world);
    Task {
        id,
        prompt,
        seed,
        expected_world,
        finish,
        tool_calls,
    }
}
