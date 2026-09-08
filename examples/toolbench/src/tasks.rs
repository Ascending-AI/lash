use serde::Serialize;
use serde_json::{Value, json};

use crate::world::{MailMessage, World};

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
            "Call weather.lookup for Berlin, then finish with the plain string 12 (the temperature in Celsius), not a number. Do not call any other tool.",
            FinishMatcher::Exact(json!("12")),
            1,
        ),
        read_task(
            "weather-condition",
            "Call weather.lookup for Berlin, then finish with its condition as a plain string. Do not call any other tool.",
            FinishMatcher::Normalized("rain".to_string()),
            1,
        ),
        read_task(
            "weather-compare",
            "Use code to call weather.lookup for Berlin and Lisbon, then finish with the warmer city's name as a string.",
            FinishMatcher::Exact(json!("Lisbon")),
            2,
        ),
        read_task(
            "string-owner",
            "Call notes.render for N-7 and extract the owner from the returned text. Finish with only the owner as a string.",
            FinishMatcher::Exact(json!("Imani")),
            1,
        ),
        read_task(
            "string-token",
            "Use code to call notes.render for N-7 and extract the value after token= without the closing parenthesis. Finish with that token as a plain string.",
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
            "Use code to call mail.list and count the messages. Finish with the count as a string.",
            FinishMatcher::Exact(json!("2")),
            1,
        ),
        read_task(
            "mail-sender",
            "Call mail.list and find the message whose subject is Build. Finish with its sender as a string.",
            FinishMatcher::Exact(json!("Ada")),
            1,
        ),
        write_task(
            "mail-send",
            "Send one message to ops@example.test with subject Deploy and body Ship build 104. Finish with the returned message id as a string.",
            FinishMatcher::Exact(json!("m3")),
            1,
            append_deploy_mail,
        ),
        write_task(
            "mail-send-read",
            "Send one message to ops@example.test with subject Deploy and body Ship build 104, then call mail.list to verify it is present. Finish with the new message id as a string.",
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
            "Use one short program to read N-9 with notes.render, extract the key after key=, and retrieve it with kv.get. Finish with the retrieved value as a string.",
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
        prompt,
        seed,
        expected_world,
        finish,
        tool_calls,
    }
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
