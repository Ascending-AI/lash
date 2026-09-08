use serde::Serialize;
use serde_json::{Value, json};

use crate::world::{MailMessage, World};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "expected")]
pub(crate) enum FinishMatcher {
    Exact(Value),
    Normalized(String),
    NumericString(f64),
}

impl FinishMatcher {
    pub(crate) fn matches(&self, actual: Option<&Value>) -> bool {
        match (self, actual) {
            (Self::Exact(expected), Some(actual)) => expected == actual,
            (Self::Normalized(expected), Some(Value::String(actual))) => {
                normalize(expected) == normalize(actual)
            }
            (Self::NumericString(expected), Some(Value::String(actual))) => actual
                .parse::<f64>()
                .is_ok_and(|number| number == *expected),
            _ => false,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Exact(value) => format!("finish exactly {value}"),
            Self::Normalized(value) => format!("normalized finish equals {value:?}"),
            Self::NumericString(value) => format!("numeric string equal to {value}"),
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
    pub(crate) prompt: String,
    pub(crate) seed: World,
    pub(crate) expected_world: World,
    pub(crate) finish: FinishMatcher,
    pub(crate) tool_calls: usize,
}

impl Task {
    pub(crate) fn prompt_for(&self, standard: bool) -> String {
        build_prompt(&self.prompt, self.tool_calls, standard)
    }

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
            "Call weather.lookup for Berlin and finish with only its temperature_c value converted to a decimal string, not a number.",
            FinishMatcher::NumericString(12.0),
            1,
        ),
        read_task(
            "weather-condition",
            "Call weather.lookup for Berlin, then finish with only its condition as a plain string.",
            FinishMatcher::Normalized("rain".to_string()),
            1,
        ),
        read_task(
            "weather-compare",
            "Call weather.lookup once for Berlin and once for Lisbon, then finish with only the warmer city's name as a string, preserving its spelling.",
            FinishMatcher::Exact(json!("Lisbon")),
            2,
        ),
        read_task(
            "string-owner",
            "Call notes.render for N-7 and extract the value after owner= up to the next comma, excluding the comma. Finish with only that value as a string, nothing else, preserving its spelling.",
            FinishMatcher::Exact(json!("Imani")),
            1,
        ),
        read_task(
            "string-token",
            "Call notes.render for N-7 and extract the value after token= up to the closing parenthesis, excluding the parenthesis. Finish with only that value as a string, nothing else, preserving its spelling.",
            FinishMatcher::Exact(json!("ALPHA-17")),
            1,
        ),
        read_task(
            "kv-read",
            "Call kv.get for project, then finish with its value as a string.",
            FinishMatcher::Exact(json!("aurora")),
            1,
        ),
        write_task(
            "kv-write",
            "Use code to call kv.put with key status and value ready, then finish with the string saved.",
            FinishMatcher::Exact(json!("saved")),
            1,
            |world| {
                world.kv.insert("status".to_string(), "ready".to_string());
            },
        ),
        write_task(
            "kv-write-read",
            "Use code to store violet under theme with kv.put and verify it with kv.get. Finish with the verified value as a string.",
            FinishMatcher::Exact(json!("violet")),
            2,
            |world| {
                world.kv.insert("theme".to_string(), "violet".to_string());
            },
        ),
        read_task(
            "mail-count",
            "Use code to call mail.list and count the messages. Finish with only the count converted to a decimal string, not a number.",
            FinishMatcher::NumericString(2.0),
            1,
        ),
        read_task(
            "mail-sender",
            "Call mail.list and find the message whose subject is Build. Finish with only its sender field as a string, preserving its spelling.",
            FinishMatcher::Exact(json!("Ada")),
            1,
        ),
        write_task(
            "mail-send",
            "Call mail.send once with recipient \"ops@example.test\", subject \"Deploy\", and body \"Ship build 104\" (exactly the text inside the quotes). Finish with only the returned id field as a string.",
            FinishMatcher::Exact(json!("m3")),
            1,
            append_deploy_mail,
        ),
        write_task(
            "mail-send-read",
            "Call mail.send once with recipient \"ops@example.test\", subject \"Deploy\", and body \"Ship build 104\" (exactly the text inside the quotes), then call mail.list once to verify it is present. Finish with only the new message's id field as a string.",
            FinishMatcher::Exact(json!("m3")),
            2,
            append_deploy_mail,
        ),
        write_task(
            "weather-to-kv",
            "Use code to call weather.lookup for Lisbon and store its condition under last_weather with kv.put. Finish with the condition as a string.",
            FinishMatcher::Exact(json!("sunny")),
            2,
            |world| {
                world
                    .kv
                    .insert("last_weather".to_string(), "sunny".to_string());
            },
        ),
        read_task(
            "missing-field",
            "Use code to call contacts.get for C-17 and check whether the record contains a phone field. Finish with its phone value if present, or the string FIELD_UNAVAILABLE if absent.",
            FinishMatcher::Exact(json!("FIELD_UNAVAILABLE")),
            1,
        ),
        write_task(
            "targeted-update",
            "Call kv.put once to set project to nebula, then finish with the string nebula. Leave everything else unchanged.",
            FinishMatcher::Exact(json!("nebula")),
            1,
            |world| {
                world.kv.insert("project".to_string(), "nebula".to_string());
            },
        ),
        read_task(
            "string-to-kv-chain",
            "Call notes.render for N-9, extract the key after key= through the end of the text, and retrieve that key with kv.get. Finish with only the retrieved value field as a string, preserving its spelling.",
            FinishMatcher::Exact(json!("L7")),
            2,
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
        prompt: prompt.replace("Use code to ", ""),
        seed,
        expected_world,
        finish,
        tool_calls,
    }
}

fn build_prompt(prompt: &str, tool_calls: usize, standard: bool) -> String {
    let constraint = if standard {
        format!(
            "Call exactly {tool_calls} host tool(s) total, then call submit exactly once with the final value; leave all other world state unchanged."
        )
    } else {
        format!(
            "Use at most two code executions and exactly {tool_calls} host tool call(s) total; allow at most {} failed executions and never repeat an identical execution error; leave all other world state unchanged.",
            crate::grading::MAX_FAILED_EXECUTIONS
        )
    };
    format!("{prompt} {constraint}")
}

fn append_deploy_mail(world: &mut World) {
    world.mail.push(MailMessage {
        id: "m3".to_string(),
        sender: "me@example.test".to_string(),
        recipient: "ops@example.test".to_string(),
        subject: "Deploy".to_string(),
        body: "Ship build 104".to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_prompt_uses_shared_task_and_submit_constraint() {
        for task in task_pack() {
            let prompt = task.prompt_for(true);
            assert!(prompt.starts_with(&task.prompt));
            assert!(prompt.contains(&format!(
                "Call exactly {} host tool(s) total, then call submit exactly once",
                task.tool_calls
            )));
            assert!(!prompt.contains("code"));
            assert!(!prompt.contains("executions"));
        }
    }

    #[test]
    fn every_task_discloses_the_grading_budget() {
        let tasks = task_pack();
        assert_eq!(tasks.len(), 16);
        for task in tasks {
            assert!(
                task.prompt_for(false)
                    .contains("at most two code executions"),
                "{}",
                task.id
            );
            assert!(
                task.prompt_for(false).contains(&format!(
                    "exactly {} host tool call(s) total",
                    task.tool_calls
                )),
                "{}",
                task.id
            );
            assert!(
                task.prompt_for(false)
                    .contains("at most 2 failed executions"),
                "{}",
                task.id
            );
            assert!(
                task.prompt_for(false)
                    .contains("never repeat an identical execution error"),
                "{}",
                task.id
            );
            assert!((1..=3).contains(&task.tool_calls));
        }
    }

    #[test]
    fn lookup_prompts_do_not_supply_the_answer() {
        for task in task_pack() {
            if matches!(
                task.id,
                "weather-temperature" | "string-owner" | "string-token"
            ) {
                let answer = match &task.finish {
                    FinishMatcher::Exact(Value::String(answer)) => answer.clone(),
                    FinishMatcher::NumericString(answer) => answer.to_string(),
                    _ => panic!("lookup must finish with a string"),
                };
                assert!(
                    !task.prompt.contains(&answer),
                    "{} leaks its answer",
                    task.id
                );
            }
        }
    }

    #[test]
    fn extraction_matchers_reject_records_and_delimiters() {
        for (id, accepted, rejected) in [
            ("string-owner", "Imani", "Imani,"),
            ("string-token", "ALPHA-17", "ALPHA-17)"),
            ("string-to-kv-chain", "L7", "launch_code"),
        ] {
            let task = task_pack().into_iter().find(|task| task.id == id).unwrap();
            assert!(task.finish.matches(Some(&json!(accepted))));
            assert!(!task.finish.matches(Some(&json!(rejected))));
            assert!(!task.finish.matches(Some(&json!({"value": accepted}))));
        }
    }

    #[test]
    fn decimal_string_requests_keep_their_type_contract() {
        for (id, value) in [("weather-temperature", 12), ("mail-count", 2)] {
            let task = task_pack().into_iter().find(|task| task.id == id).unwrap();
            assert!(task.prompt.contains("decimal string, not a number"));
            assert!(task.finish.matches(Some(&json!(value.to_string()))));
            assert!(task.finish.matches(Some(&json!(format!("{value}.0")))));
            assert!(!task.finish.matches(Some(&json!(value))));
            assert!(!task.finish.matches(Some(&json!((value + 1).to_string()))));
            assert!(!task.finish.matches(Some(&json!("NaN"))));
            assert!(
                !task
                    .finish
                    .matches(Some(&json!(format!("{value} degrees"))))
            );
        }
    }

    #[test]
    fn mail_prompts_quote_the_exact_body_without_sentence_punctuation() {
        for task in task_pack()
            .into_iter()
            .filter(|task| matches!(task.id, "mail-send" | "mail-send-read"))
        {
            assert!(task.prompt.contains("body \"Ship build 104\""));
            let mut evidence = crate::grading::RunEvidence {
                completed: true,
                finish_value: Some(json!("m3")),
                tool_call_count: task.tool_calls,
                ..Default::default()
            };
            assert!(crate::grading::grade(&task, &task.expected_world, &evidence).passed);
            let mut wrong = task.expected_world.clone();
            wrong.mail.last_mut().unwrap().body.push('.');
            assert!(!crate::grading::grade(&task, &wrong, &evidence).passed);
            evidence.finish_value = Some(json!(task.expected_world.mail.last().unwrap()));
            assert!(!crate::grading::grade(&task, &task.expected_world, &evidence).passed);
        }
    }
}
