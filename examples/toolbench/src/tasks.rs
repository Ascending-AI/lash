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
            "{}; exact seeded-world equality; exactly {} tool call(s); turn completes; at most 2 failed executions; no repeated identical execution error",
            self.finish.describe(),
            self.tool_calls
        )
    }
}

pub(crate) fn task_pack() -> Vec<Task> {
    vec![
        read_task(
            "weather-temperature",
            "Call weather.lookup for Berlin, then finish with the plain string 12 (the temperature in Celsius), not a number. Do not JSON-encode or wrap the string. Do not call any other tool.",
            FinishMatcher::Exact(json!("12")),
            1,
        ),
        read_task(
            "weather-condition",
            "Call weather.lookup for Berlin, then finish with its condition as a plain string. Do not JSON-encode or wrap the string. Do not call any other tool.",
            FinishMatcher::Normalized("rain".to_string()),
            1,
        ),
        read_task(
            "weather-compare",
            "Call weather.lookup for Berlin and Lisbon. Finish with the warmer city's name as a plain string. Do not JSON-encode or wrap the string. Call no other tools.",
            FinishMatcher::Exact(json!("Lisbon")),
            2,
        ),
        read_task(
            "string-owner",
            "Call notes.render for N-7. Its result is a STRING, even though it looks like a record. Parse the text and finish with only the owner as a plain string. Do not JSON-encode or wrap the string. Call no other tools.",
            FinishMatcher::Exact(json!("Imani")),
            1,
        ),
        read_task(
            "string-token",
            "Call notes.render for N-7. Treat the result as a string and finish with only its token as a plain string. Do not JSON-encode or wrap the string. Call no other tools.",
            FinishMatcher::Exact(json!("ALPHA-17")),
            1,
        ),
        read_task(
            "kv-read",
            "Call kv.get for project and finish with only its value as a plain string. Do not JSON-encode or wrap the string. Call no other tools.",
            FinishMatcher::Exact(json!("aurora")),
            1,
        ),
        write_task(
            "kv-write",
            "Call kv.put once to store key status with value ready. Finish with the plain string saved. Do not JSON-encode or wrap the string. Change nothing else.",
            FinishMatcher::Exact(json!("saved")),
            1,
            |world| {
                world.kv.insert("status".to_string(), "ready".to_string());
            },
        ),
        write_task(
            "kv-write-read",
            "Call kv.put to change theme to violet, then call kv.get for theme to verify the write. Finish with only the verified value as a plain string. Do not JSON-encode or wrap the string. Change nothing else and make exactly those two tool calls.",
            FinishMatcher::Exact(json!("violet")),
            2,
            |world| {
                world.kv.insert("theme".to_string(), "violet".to_string());
            },
        ),
        read_task(
            "mail-count",
            "Call mail.list once and finish with the plain string 2, the number of messages, not a number. Do not JSON-encode or wrap the string. Call no other tools.",
            FinishMatcher::Exact(json!("2")),
            1,
        ),
        read_task(
            "mail-sender",
            "Call mail.list once, find the message with subject Build, and finish with only its sender as a plain string. Do not JSON-encode or wrap the string. Call no other tools.",
            FinishMatcher::Exact(json!("Ada")),
            1,
        ),
        write_task(
            "mail-send",
            "Call mail.send once with recipient ops@example.test, subject Deploy, and body Ship build 104. Finish with only the returned id as a plain string. Do not JSON-encode or wrap the string. Send nothing else.",
            FinishMatcher::Exact(json!("m3")),
            1,
            append_deploy_mail,
        ),
        write_task(
            "mail-send-read",
            "Call mail.send once with recipient ops@example.test, subject Deploy, and body Ship build 104. Then call mail.list once to verify it is present. Finish with only the new id as a plain string. Do not JSON-encode or wrap the string. Make exactly those two calls.",
            FinishMatcher::Exact(json!("m3")),
            2,
            append_deploy_mail,
        ),
        write_task(
            "weather-to-kv",
            "Call weather.lookup for Lisbon, then call kv.put to store its condition under key last_weather. Finish with only the stored condition as a plain string. Do not JSON-encode or wrap the string. Change nothing else.",
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
            "Call contacts.get for C-17. If and only if the returned structured record has no phone field, finish with the plain string FIELD_UNAVAILABLE. Do not JSON-encode or wrap the string. Never invent a phone number and call no other tools.",
            FinishMatcher::Exact(json!("FIELD_UNAVAILABLE")),
            1,
        ),
        write_task(
            "targeted-update",
            "Call kv.put once to change only project from aurora to nebula. Finish with the plain string nebula. Do not JSON-encode or wrap the string. Preserve every other key and all non-KV records.",
            FinishMatcher::Exact(json!("nebula")),
            1,
            |world| {
                world.kv.insert("project".to_string(), "nebula".to_string());
            },
        ),
        read_task(
            "string-to-kv-chain",
            "Call notes.render for N-9. Its result is a STRING containing a key name. Extract that key, call kv.get with it, and finish with only the retrieved value as a plain string. Do not JSON-encode or wrap the string. Make exactly those two calls.",
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
