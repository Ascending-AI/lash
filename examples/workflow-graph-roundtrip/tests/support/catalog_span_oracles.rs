pub(super) fn expected_catalog_node_slices(id: &str) -> &'static [&'static str] {
    match id {
        "blank" => &["return 0;"],
        "branching-approval" => &[
            r#"await (display.set_status({ key: "approval", value: "waiting" }))"#,
            r#"await (display.highlight({ target: "approval" }))"#,
            r#"await (display.show_message({ text: "Approval requested" }))"#,
            r#"let decision = await waitSignal("continue");"#,
            "if (decision.autoFired) {\n    await (display.set_status({ key: \"approval\", value: \"approved\" }));\n    if (true) {\n      await (display.set_light({ name: \"approved\", state: \"green\" }));\n      await (display.show_message({ text: \"Request approved\" }));\n    } else {\n      await (display.show_message({ text: \"Approval needs review\" }));\n    }\n  } else {\n    await (display.set_status({ key: \"approval\", value: \"rejected\" }));\n    await (display.set_light({ name: \"rejected\", state: \"red\" }));\n    await (display.show_message({ text: \"Request rejected\" }));\n  }",
            r#"await (display.set_status({ key: "approval", value: "approved" }))"#,
            "if (true) {\n      await (display.set_light({ name: \"approved\", state: \"green\" }));\n      await (display.show_message({ text: \"Request approved\" }));\n    } else {\n      await (display.show_message({ text: \"Approval needs review\" }));\n    }",
            r#"await (display.set_light({ name: "approved", state: "green" }))"#,
            r#"await (display.show_message({ text: "Request approved" }))"#,
            r#"await (display.show_message({ text: "Approval needs review" }))"#,
            r#"await (display.set_status({ key: "approval", value: "rejected" }))"#,
            r#"await (display.set_light({ name: "rejected", state: "red" }))"#,
            r#"await (display.show_message({ text: "Request rejected" }))"#,
            r#"sleep("400ms")"#,
            r#"await (display.highlight({ target: "result" }))"#,
            "return decision;",
        ],
        "counter-loop" => &[
            r#"await (display.set_status({ key: "counter", value: "running" }))"#,
            "await (display.set_progress({ pct: 5 }))",
            "let state = { count: 0 };",
            "while ((state.count < 3)) {\n    await (display.add_item({ list: \"counts\", item: state.count }));\n    await (display.set_progress({ pct: ((state.count * 20) + 20) }));\n    state.count = (state.count + 1);\n    await sleep(\"250ms\");\n  }",
            r#"await (display.add_item({ list: "counts", item: state.count }))"#,
            "await (display.set_progress({ pct: ((state.count * 20) + 20) }))",
            "state.count = (state.count + 1);",
            r#"sleep("250ms")"#,
            "for (const pct of [70, 85, 100]) {\n    await (display.set_progress({ pct: pct }));\n    await sleep(\"300ms\");\n  }",
            "await (display.set_progress({ pct: pct }))",
            r#"sleep("300ms")"#,
            r#"await (display.highlight({ target: "progress" }))"#,
            r#"await (display.set_status({ key: "counter", value: "complete" }))"#,
            r#"await (display.show_message({ text: "Counter complete" }))"#,
            "return state.count;",
        ],
        "onboarding" => &[
            r#"await (display.set_status({ key: "phase", value: "starting" }));"#,
            r#"sleep("400ms")"#,
            r#"await (display.show_message({ text: "Welcome to the workflow graph" }))"#,
            r#"await (display.set_light({ name: "ready", state: "green" }))"#,
            r#"sleep("400ms")"#,
            "if (true) {\n    await (display.set_progress({ pct: 35 }));\n  } else {\n    await (display.show_message({ text: \"Alternate path\" }));\n  }",
            "await (display.set_progress({ pct: 35 }))",
            r#"await (display.show_message({ text: "Alternate path" }))"#,
            r#"let approval = await waitSignal("continue");"#,
            r#"await (display.highlight({ target: "checklist" }))"#,
            r#"await (display.add_item({ list: "steps", item: "Approved" }))"#,
            "let count = 0;",
            "while ((count < 2)) {\n    await (display.add_item({ list: \"steps\", item: \"Loop item\" }));\n    count = (count + 1);\n    await sleep(\"250ms\");\n  }",
            r#"await (display.add_item({ list: "steps", item: "Loop item" }))"#,
            "count = (count + 1);",
            r#"sleep("250ms")"#,
            r#"sleep("400ms")"#,
            "await (display.set_progress({ pct: 100 }))",
            r#"await (display.set_light({ name: "complete", state: "blue" }))"#,
            "return approval;",
        ],
        "research-nvidia-stock" => &[
            r#"let search = await (web.search({ query: "NVIDIA stock outlook" }));"#,
            r#"let research = await (agents.spawn({ capability: "explore", task: "Research NVIDIA's stock outlook from the supplied web search results", seed: { search_results: search.results } }));"#,
            "await (display.show_message({ text: research.summary }))",
            "return research;",
        ],
        "summarize-emails" => &[
            "let emails = await (gmail.list_recent({ count: 5 }));",
            "for (const email of emails) {\n    await (llm.query({ task: \"Summarize this email in one sentence\", inputs: { sender: email[\"from\"], subject: email.subject, snippet: email.snippet } }));\n  }",
            r#"await (llm.query({ task: "Summarize this email in one sentence", inputs: { sender: email["from"], subject: email.subject, snippet: email.snippet } }))"#,
            r#"let digest = await (llm.query({ task: "Format these five summaries as a concise numbered email digest", inputs: { summaries: emails } }));"#,
            "await (display.show_message({ text: digest }))",
            "return digest;",
        ],
        "team-standup-digest" => &[
            r#"let messages = await (slack.recent({ channel: "team-platform", since: "yesterday" }));"#,
            r#"let activity = await (github.recent({ repo: "acme/widgets", since: "yesterday" }));"#,
            r#"let standup = await (agents.spawn({ capability: "peer", task: "Synthesize a concise team standup digest and call out blockers", seed: { slack_messages: messages, github_activity: activity } }));"#,
            "await (display.show_message({ text: standup.digest }))",
            "return standup.digest;",
        ],
        "traffic-lights" => &[
            r#"await (display.set_status({ key: "traffic", value: "running" }))"#,
            "for (const cycle of [1, 2]) {\n    await (display.add_item({ list: \"cycles\", item: cycle }));\n    /** @label Red — Stop the traffic */\n    await (display.set_light({ name: \"red\", state: \"on\" }));\n    await (display.set_light({ name: \"amber\", state: \"off\" }));\n    await (display.set_light({ name: \"green\", state: \"off\" }));\n    await sleep(\"350ms\");\n    await (display.set_light({ name: \"red\", state: \"off\" }));\n    await (display.set_light({ name: \"amber\", state: \"on\" }));\n    await sleep(\"350ms\");\n    await (display.set_light({ name: \"amber\", state: \"off\" }));\n    await (display.set_light({ name: \"green\", state: \"on\" }));\n    await (display.show_message({ text: \"Go\" }));\n    await sleep(\"500ms\");\n  }",
            r#"await (display.add_item({ list: "cycles", item: cycle }))"#,
            r#"await (display.set_light({ name: "red", state: "on" }));"#,
            r#"await (display.set_light({ name: "amber", state: "off" }))"#,
            r#"await (display.set_light({ name: "green", state: "off" }))"#,
            r#"sleep("350ms")"#,
            r#"await (display.set_light({ name: "red", state: "off" }))"#,
            r#"await (display.set_light({ name: "amber", state: "on" }))"#,
            r#"sleep("350ms")"#,
            r#"await (display.set_light({ name: "amber", state: "off" }))"#,
            r#"await (display.set_light({ name: "green", state: "on" }))"#,
            r#"await (display.show_message({ text: "Go" }))"#,
            r#"sleep("500ms")"#,
            r#"await (display.set_status({ key: "traffic", value: "complete" }))"#,
            "return null;",
        ],
        other => panic!("missing exact-slice oracle for catalog workflow `{other}`"),
    }
}

pub(super) fn expected_catalog_process_slice<'a>(id: &str, source: &'a str) -> &'a str {
    let leading_label = match id {
        "onboarding" => {
            Some("/** @label Onboarding — Welcome a new operator and wait for their approval */\n")
        }
        "traffic-lights" => {
            Some("/** @label Traffic lights — Cycle a three-light signal twice */\n")
        }
        "branching-approval" => Some(
            "/** @label Branching approval — Wait for a decision and take one of two paths */\n",
        ),
        "counter-loop" => {
            Some("/** @label Counter loop — Count to three, then walk a fixed progress list */\n")
        }
        "blank" | "summarize-emails" | "research-nvidia-stock" | "team-standup-digest" => None,
        other => panic!("missing process-root oracle for catalog workflow `{other}`"),
    };
    leading_label
        .map_or(source, |label| {
            source
                .strip_prefix(label)
                .unwrap_or_else(|| panic!("catalog workflow `{id}` changed its leading label"))
        })
        .trim_end()
}
