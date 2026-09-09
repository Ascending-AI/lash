use serde::Serialize;
use serde_json::{Value, json};

use crate::world::{MailMessage, World};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "expected")]
pub(crate) enum FinishMatcher {
    Exact(Value),
    UnorderedSet(Vec<Value>),
    Normalized(String),
    Numeric(f64),
}

impl FinishMatcher {
    pub(crate) fn matches(&self, actual: Option<&Value>) -> bool {
        match (self, actual) {
            (Self::Exact(expected), Some(actual)) => expected == actual,
            (Self::UnorderedSet(expected), Some(Value::Array(actual))) => {
                actual.iter().all(|v| expected.contains(v))
                    && expected.iter().all(|v| actual.contains(v))
            }
            (Self::Normalized(expected), Some(Value::String(actual))) => {
                normalize(expected) == normalize(actual)
            }
            (Self::Numeric(expected), Some(Value::Number(actual))) => actual
                .as_f64()
                .is_some_and(|number| number.is_finite() && number == *expected),
            (Self::Numeric(expected), Some(Value::String(actual))) => actual
                .parse::<f64>()
                .is_ok_and(|number| number.is_finite() && number == *expected),
            _ => false,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Exact(value) => format!("finish exactly {value}"),
            Self::UnorderedSet(values) => format!("finish set equal to {}", json!(values)),
            Self::Normalized(value) => format!("normalized finish equals {value:?}"),
            Self::Numeric(value) => format!("numeric value equal to {value}"),
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
        build_prompt(&self.prompt, standard)
    }

    pub(crate) fn checker_description(&self) -> String {
        format!(
            "{}; exact seeded-world equality; turn completes with finish/submit",
            self.finish.describe()
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Pack {
    Easy,
    Hard,
    All,
}

impl Task {
    pub(crate) fn pack(&self) -> Pack {
        if self.id.starts_with("hard-") {
            Pack::Hard
        } else {
            Pack::Easy
        }
    }
}

pub(crate) fn task_pack() -> Vec<Task> {
    let mut tasks = easy_pack();
    tasks.extend(hard_pack());
    tasks
}

pub(crate) fn easy_pack() -> Vec<Task> {
    vec![
        read_task(
            "weather-temperature",
            "Call weather.lookup for Berlin and finish with only its temperature_c value.",
            FinishMatcher::Numeric(12.0),
            1,
        ),
        read_task(
            "weather-condition",
            "Call weather.lookup for Berlin, then finish with only its condition.",
            FinishMatcher::Normalized("rain".to_string()),
            1,
        ),
        read_task(
            "weather-compare",
            "Call weather.lookup once for Berlin and once for Lisbon, then finish with only the warmer city's name, preserving its spelling.",
            FinishMatcher::Exact(json!("Lisbon")),
            2,
        ),
        read_task(
            "string-owner",
            "Call notes.render for N-7 and extract the value after owner= up to the next comma, excluding the comma. Finish with only that value, nothing else, preserving its spelling.",
            FinishMatcher::Exact(json!("Imani")),
            1,
        ),
        read_task(
            "string-token",
            "Call notes.render for N-7 and extract the value after token= up to the closing parenthesis, excluding the parenthesis. Finish with only that value, nothing else, preserving its spelling.",
            FinishMatcher::Exact(json!("ALPHA-17")),
            1,
        ),
        read_task(
            "kv-read",
            "Call kv.get for project, then finish with its value.",
            FinishMatcher::Exact(json!("aurora")),
            1,
        ),
        write_task(
            "kv-write",
            "Use code to call kv.put with key status and value ready, then finish with saved.",
            FinishMatcher::Exact(json!("saved")),
            1,
            |world| {
                world.kv.insert("status".to_string(), "ready".to_string());
            },
        ),
        write_task(
            "kv-write-read",
            "Use code to store violet under theme with kv.put and verify it with kv.get. Finish with the verified value.",
            FinishMatcher::Exact(json!("violet")),
            2,
            |world| {
                world.kv.insert("theme".to_string(), "violet".to_string());
            },
        ),
        read_task(
            "mail-count",
            "Use code to call mail.list and count the messages. Finish with only the count.",
            FinishMatcher::Numeric(2.0),
            1,
        ),
        read_task(
            "mail-sender",
            "Call mail.list and find the message whose subject is Build. Finish with only its sender field, preserving its spelling.",
            FinishMatcher::Exact(json!("Ada")),
            1,
        ),
        write_task(
            "mail-send",
            "Call mail.send once with recipient \"ops@example.test\", subject \"Deploy\", and body \"Ship build 104\" (exactly the text inside the quotes). Finish with only the returned id field.",
            FinishMatcher::Exact(json!("m3")),
            1,
            append_deploy_mail,
        ),
        write_task(
            "mail-send-read",
            "Call mail.send once with recipient \"ops@example.test\", subject \"Deploy\", and body \"Ship build 104\" (exactly the text inside the quotes), then call mail.list once to verify it is present. Finish with only the new message's id field.",
            FinishMatcher::Exact(json!("m3")),
            2,
            append_deploy_mail,
        ),
        write_task(
            "weather-to-kv",
            "Use code to call weather.lookup for Lisbon and store its condition under last_weather with kv.put. Finish with the condition.",
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
            "Use code to call contacts.get for C-17 and check whether the record contains a phone field. Finish with its phone value if present, or FIELD_UNAVAILABLE if absent.",
            FinishMatcher::Exact(json!("FIELD_UNAVAILABLE")),
            1,
        ),
        write_task(
            "targeted-update",
            "Call kv.put once to set project to nebula, then finish with nebula. Leave everything else unchanged.",
            FinishMatcher::Exact(json!("nebula")),
            1,
            |world| {
                world.kv.insert("project".to_string(), "nebula".to_string());
            },
        ),
        read_task(
            "string-to-kv-chain",
            "Call notes.render for N-9, extract the key after key= through the end of the text, and retrieve that key with kv.get. Finish with only the retrieved value field, preserving its spelling.",
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
    let mut seed = World::seeded();
    seed.catalog = if id.starts_with("hard-retail-") {
        crate::world::Catalog::Retail
    } else if id.starts_with("hard-ops-") {
        crate::world::Catalog::Ops
    } else {
        crate::world::Catalog::Easy
    };
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

fn build_prompt(prompt: &str, standard: bool) -> String {
    let prompt = standard_tool_names(prompt, standard);
    let constraint = if standard {
        "Call submit exactly once with the final value; leave all other world state unchanged."
    } else {
        "Leave all other world state unchanged."
    };
    format!("{prompt} {constraint}")
}

fn standard_tool_names(prompt: &str, standard: bool) -> String {
    if !standard {
        return prompt.to_string();
    }

    [
        ("weather.lookup", "weather_lookup"),
        ("kv.get", "kv_get"),
        ("kv.put", "kv_put"),
        ("notes.render", "notes_render"),
        ("mail.list", "mail_list"),
        ("mail.send", "mail_send"),
        ("contacts.get", "contacts_get"),
    ]
    .into_iter()
    .fold(prompt.to_string(), |prompt, (display, registered)| {
        prompt.replace(display, registered)
    })
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
            assert!(prompt.contains("Call submit exactly once with the final value"));
        }
    }

    #[test]
    fn standard_prompt_uses_registered_tool_names() {
        let mappings = [
            ("weather.lookup", "weather_lookup"),
            ("kv.get", "kv_get"),
            ("kv.put", "kv_put"),
            ("notes.render", "notes_render"),
            ("mail.list", "mail_list"),
            ("mail.send", "mail_send"),
            ("contacts.get", "contacts_get"),
        ];
        for task in task_pack() {
            let prompt = task.prompt_for(true);
            for (display, registered) in mappings {
                if task.prompt.contains(display) {
                    assert!(prompt.contains(registered), "{}", task.id);
                    assert!(!prompt.contains(display), "{}", task.id);
                }
            }
        }
    }

    #[test]
    fn shared_prompts_only_append_completion_and_world_constraints() {
        let tasks = task_pack();
        assert_eq!(tasks.len(), 28);
        for task in tasks {
            assert_eq!(
                task.prompt_for(false),
                format!("{} Leave all other world state unchanged.", task.prompt)
            );
            assert_eq!(
                task.prompt_for(true),
                format!(
                    "{} Call submit exactly once with the final value; leave all other world state unchanged.",
                    standard_tool_names(&task.prompt, true)
                )
            );
            assert!(match task.pack() {
                Pack::Easy => (1..=3).contains(&task.tool_calls),
                Pack::Hard => (4..=10).contains(&task.tool_calls),
                Pack::All => false,
            });
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
                    FinishMatcher::Numeric(answer) => answer.to_string(),
                    _ => panic!("lookup must finish with a string"),
                };
                assert!(
                    ![false, true]
                        .iter()
                        .any(|standard| task.prompt_for(*standard).contains(&answer)),
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
    fn numeric_requests_accept_numbers_and_numeric_strings() {
        for (id, value) in [("weather-temperature", 12), ("mail-count", 2)] {
            let task = task_pack().into_iter().find(|task| task.id == id).unwrap();
            assert!(!task.prompt.contains("decimal string"));
            assert!(!task.prompt.contains("not a number"));
            assert!(task.finish.matches(Some(&json!(value))));
            assert!(task.finish.matches(Some(&json!(value.to_string()))));
            assert!(task.finish.matches(Some(&json!(format!("{value}.0")))));
            assert!(!task.finish.matches(Some(&json!(value + 1))));
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
    fn numeric_matcher_accepts_equivalent_forms_and_rejects_non_numbers() {
        let matcher = FinishMatcher::Numeric(12.0);
        for actual in [
            json!(12),
            json!(12.0),
            json!("12"),
            json!("12.0"),
            json!("+12"),
            json!("1.2e1"),
        ] {
            assert!(matcher.matches(Some(&actual)), "{actual}");
        }
        for actual in [
            json!(13),
            json!("13"),
            json!("12 degrees"),
            json!("NaN"),
            Value::Null,
            json!({"value": 12}),
        ] {
            assert!(!matcher.matches(Some(&actual)), "{actual}");
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
            assert!(crate::grading::grade(&task, &task.expected_world, &evidence, 0.10).passed);
            let mut wrong = task.expected_world.clone();
            wrong.mail.last_mut().unwrap().body.push('.');
            assert!(!crate::grading::grade(&task, &wrong, &evidence, 0.10).passed);
            evidence.finish_value = Some(json!(task.expected_world.mail.last().unwrap()));
            assert!(!crate::grading::grade(&task, &task.expected_world, &evidence, 0.10).passed);
        }
    }
}

pub(crate) fn hard_pack() -> Vec<Task> {
    vec![
        write_task(
            "hard-retail-refund",
            "Refund all of Mira's delivered orders within the 30-day return window. Finish with only the total refunded cents verified in those orders, nothing else.",
            FinishMatcher::Numeric(4000.0),
            9,
            |w| {
                w.retail.orders[0].refunded_cents = 2200;
                w.retail.orders[2].refunded_cents = 1800;
            },
        ),
        write_task(
            "hard-retail-exchange",
            "For order R7, attempt the requested replacement P2; if it is out of stock, use the cheapest other product in the original category with enough stock for the entire order. Finish with only the verified replacement sku field value, nothing else.",
            FinishMatcher::Exact(json!("P5")),
            6,
            |w| {
                w.retail.orders[6].sku = "P5".into();
                w.retail.products[4].stock = 0;
            },
        ),
        write_task(
            "hard-retail-reschedule",
            "Move Sana's earliest pending delivery to day 22 only if she has no pending payment; shipped orders must stay unchanged. Finish with only the verified delivery_day field value of the moved order, or PAYMENT_PENDING if payment blocks it, nothing else.",
            FinishMatcher::Numeric(22.0),
            7,
            |w| {
                w.retail.orders[6].delivery_day = 22;
            },
        ),
        read_task(
            "hard-retail-reprice",
            "How many cents more would Mira pay at current product prices for the quantities in her delivered orders still within the 30-day return window, compared with what she actually paid for those orders? Finish with only the difference in cents, nothing else.",
            FinishMatcher::Numeric(500.0),
            7,
        ),
        read_task(
            "hard-retail-lamps",
            "Which of Mira's delivered orders contain products in category lamp? Finish with only their order IDs, nothing else, without changing anything.",
            FinishMatcher::UnorderedSet(vec![json!("R1"), json!("R2")]),
            8,
        ),
        read_task(
            "hard-retail-best-return",
            "Which of Mira's delivered orders still within the 30-day return window has the greatest paid amount per unit? Break ties by the earlier order ID and finish with only that order ID, nothing else.",
            FinishMatcher::Exact(json!("R1")),
            5,
        ),
        write_task(
            "hard-ops-deploy-recovery",
            "Attempt Beacon's newest release with passed checks; if capacity prevents it, deploy the newest passed release that fits. Finish with only the verified deployed_release field value, nothing else.",
            FinishMatcher::Exact(json!("V2")),
            8,
            |w| {
                w.ops.services[0].deployed_release = "V2".into();
            },
        ),
        write_task(
            "hard-ops-resolve-chain",
            "Resolve incident I2 and its open prerequisites with their required fixes deployed on Beacon. Finish with only I2's verified status field value, nothing else.",
            FinishMatcher::Exact(json!("resolved")),
            9,
            |w| {
                w.ops.services[0].deployed_release = "V2".into();
                w.ops.tickets[0].status = "resolved".into();
                w.ops.tickets[1].status = "resolved".into();
            },
        ),
        read_task(
            "hard-ops-impact",
            "What is the sum of affected users on Beacon's open incidents of severity 1 or 2? Exclude resolved incidents and finish with only the total, nothing else.",
            FinishMatcher::Numeric(200.0),
            5,
        ),
        read_task(
            "hard-ops-oncall",
            "For the highest-severity open incident across Beacon and Harbor, finish with only the owning team's available primary email, or its backup email when the primary is unavailable. Break severity ties by the larger affected-user count and include nothing besides that email.",
            FinishMatcher::Exact(json!("nia@example.test")),
            10,
        ),
        read_task(
            "hard-ops-ready",
            "Which of Beacon's open incidents have no open prerequisites and a fix release whose checks passed and capacity fits Beacon? Finish with only the incident IDs, nothing else, without deploying or resolving anything.",
            FinishMatcher::UnorderedSet(vec![json!("I1")]),
            6,
        ),
        read_task(
            "hard-ops-blocked-impact",
            "Among incident I2 and all of Harbor's incidents, how many affected users belong to open incidents whose required fix release has failed checks? Finish with only the total, nothing else.",
            FinishMatcher::Numeric(290.0),
            7,
        ),
    ]
}

#[cfg(test)]
#[path = "hard_tests.rs"]
mod hard_tests;
