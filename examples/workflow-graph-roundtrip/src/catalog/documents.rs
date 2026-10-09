//! The built-in workflows in kernel notation. `@{name}` stands for the
//! identity the host's environment gives the library function `name`.

pub(super) const BLANK: &str = r#"kernel 1
numbers float
entry blank() -> Any

fn blank() {
  return 0
}

main {
  finish null
}
"#;

pub(super) const BRANCHING_APPROVAL: &str = r#"kernel 1
numbers float
effect display.highlight(input: Any) -> Any
effect display.set_light(input: Any) -> Any
effect display.set_status(input: Any) -> Any
effect display.show_message(input: Any) -> Any
effect host.approval(input: Any) -> Any
entry branching_approval() -> Any

fn branching_approval() {
  let shown_1 = {key: "approval", value: "waiting"}
  do perform display.set_status(shown_1) as Any
  let shown_2 = {target: "approval"}
  do perform display.highlight(shown_2) as Any
  let shown_3 = {text: "Approval requested"}
  do perform display.show_message(shown_3) as Any
  let request = {}
  let decision = perform host.approval(request) as Any
  if decision.approved {
    let shown_4 = {key: "approval", value: "approved"}
    do perform display.set_status(shown_4) as Any
    if true {
      let shown_5 = {name: "approved", state: "green"}
      do perform display.set_light(shown_5) as Any
      let shown_6 = {text: "Request approved"}
      do perform display.show_message(shown_6) as Any
    } else {
      let shown_7 = {text: "Approval needs review"}
      do perform display.show_message(shown_7) as Any
    }
  } else {
    let shown_8 = {key: "approval", value: "rejected"}
    do perform display.set_status(shown_8) as Any
    let shown_9 = {name: "rejected", state: "red"}
    do perform display.set_light(shown_9) as Any
    let shown_10 = {text: "Request rejected"}
    do perform display.show_message(shown_10) as Any
  }
  do sleep 400
  let shown_11 = {target: "result"}
  do perform display.highlight(shown_11) as Any
  return decision
}

main {
  finish null
}
"#;

pub(super) const COUNTER_LOOP: &str = r#"kernel 1
numbers float
effect display.add_item(input: Any) -> Any
effect display.highlight(input: Any) -> Any
effect display.set_progress(input: Any) -> Any
effect display.set_status(input: Any) -> Any
effect display.show_message(input: Any) -> Any
use num.lt = @{num.lt}
use num.add = @{num.add}
entry counter_loop() -> Any

fn counter_loop() {
  let shown_1 = {key: "counter", value: "running"}
  do perform display.set_status(shown_1) as Any
  let shown_2 = {pct: 5}
  do perform display.set_progress(shown_2) as Any
  let count = 0
  let reached = 20
  while num.lt(count, 3) {
    let shown_3 = {list: "counts", item: count}
    do perform display.add_item(shown_3) as Any
    let shown_4 = {pct: reached}
    do perform display.set_progress(shown_4) as Any
    set count = num.add(count, 1)
    set reached = num.add(reached, 20)
    do sleep 250
  }
  for pct in [70, 85, 100] {
    let shown_5 = {pct: pct}
    do perform display.set_progress(shown_5) as Any
    do sleep 300
  }
  let shown_6 = {target: "progress"}
  do perform display.highlight(shown_6) as Any
  let shown_7 = {key: "counter", value: "complete"}
  do perform display.set_status(shown_7) as Any
  let shown_8 = {text: "Counter complete"}
  do perform display.show_message(shown_8) as Any
  return count
}

main {
  finish null
}
"#;

pub(super) const ONBOARDING: &str = r#"kernel 1
numbers float
effect display.add_item(input: Any) -> Any
effect display.highlight(input: Any) -> Any
effect display.set_light(input: Any) -> Any
effect display.set_progress(input: Any) -> Any
effect display.set_status(input: Any) -> Any
effect display.show_message(input: Any) -> Any
effect host.approval(input: Any) -> Any
use num.lt = @{num.lt}
use num.add = @{num.add}
entry onboarding() -> Any

fn onboarding() {
  let shown_1 = {key: "phase", value: "starting"}
  do perform display.set_status(shown_1) as Any
  do sleep 400
  let shown_2 = {text: "Welcome to the workflow graph"}
  do perform display.show_message(shown_2) as Any
  let shown_3 = {name: "ready", state: "green"}
  do perform display.set_light(shown_3) as Any
  do sleep 400
  if true {
    let shown_4 = {pct: 35}
    do perform display.set_progress(shown_4) as Any
  } else {
    let shown_5 = {text: "Alternate path"}
    do perform display.show_message(shown_5) as Any
  }
  let request = {}
  let approval = perform host.approval(request) as Any
  let shown_6 = {target: "checklist"}
  do perform display.highlight(shown_6) as Any
  let shown_7 = {list: "steps", item: "Approved"}
  do perform display.add_item(shown_7) as Any
  let count = 0
  while num.lt(count, 2) {
    let shown_8 = {list: "steps", item: "Loop item"}
    do perform display.add_item(shown_8) as Any
    set count = num.add(count, 1)
    do sleep 250
  }
  do sleep 400
  let shown_9 = {pct: 100}
  do perform display.set_progress(shown_9) as Any
  let shown_10 = {name: "complete", state: "blue"}
  do perform display.set_light(shown_10) as Any
  return approval
}

main {
  finish null
}
"#;

pub(super) const RESEARCH_NVIDIA_STOCK: &str = r#"kernel 1
numbers float
effect agents.spawn(input: Any) -> Any
effect display.show_message(input: Any) -> Any
effect web.search(input: Any) -> Any
entry research_nvidia_stock() -> Any

fn research_nvidia_stock() {
  let query = {query: "NVIDIA stock outlook"}
  let search = perform web.search(query) as Any
  let brief = {capability: "explore", task: "Research NVIDIA's stock outlook from the supplied web search results", seed: {search_results: search.results}}
  let research = perform agents.spawn(brief) as Any
  let shown_1 = {text: research.summary}
  do perform display.show_message(shown_1) as Any
  return research
}

main {
  finish null
}
"#;

pub(super) const SUMMARIZE_TOP_EMAILS: &str = r#"kernel 1
numbers float
effect display.show_message(input: Any) -> Any
effect gmail.list_recent(input: Any) -> Any
effect llm.query(input: Any) -> Any
entry summarize_top_emails() -> Any

fn summarize_top_emails() {
  let recent = {count: 5}
  let emails = perform gmail.list_recent(recent) as Any
  for email in emails {
    let one = {task: "Summarize this email in one sentence", inputs: {sender: email.sender, subject: email.subject, snippet: email.snippet}}
    do perform llm.query(one) as Any
  }
  let all = {task: "Format these five summaries as a concise numbered email digest", inputs: {summaries: emails}}
  let digest = perform llm.query(all) as Any
  let shown_1 = {text: digest}
  do perform display.show_message(shown_1) as Any
  return digest
}

main {
  finish null
}
"#;

pub(super) const TEAM_STANDUP_DIGEST: &str = r#"kernel 1
numbers float
effect agents.spawn(input: Any) -> Any
effect display.show_message(input: Any) -> Any
effect github.recent(input: Any) -> Any
effect slack.recent(input: Any) -> Any
entry team_standup_digest() -> Any

fn team_standup_digest() {
  let channel = {channel: "team-platform", since: "yesterday"}
  let messages = perform slack.recent(channel) as Any
  let repo = {repo: "acme/widgets", since: "yesterday"}
  let activity = perform github.recent(repo) as Any
  let brief = {capability: "peer", task: "Synthesize a concise team standup digest and call out blockers", seed: {slack_messages: messages, github_activity: activity}}
  let standup = perform agents.spawn(brief) as Any
  let shown_1 = {text: standup.digest}
  do perform display.show_message(shown_1) as Any
  return standup.digest
}

main {
  finish null
}
"#;

pub(super) const TRAFFIC_LIGHTS: &str = r#"kernel 1
numbers float
effect display.add_item(input: Any) -> Any
effect display.set_light(input: Any) -> Any
effect display.set_status(input: Any) -> Any
effect display.show_message(input: Any) -> Any
entry traffic_lights() -> Any

fn traffic_lights() {
  let shown_1 = {key: "traffic", value: "running"}
  do perform display.set_status(shown_1) as Any
  for cycle in [1, 2] {
    let shown_2 = {list: "cycles", item: cycle}
    do perform display.add_item(shown_2) as Any
    let shown_3 = {name: "red", state: "on"}
    do perform display.set_light(shown_3) as Any
    let shown_4 = {name: "amber", state: "off"}
    do perform display.set_light(shown_4) as Any
    let shown_5 = {name: "green", state: "off"}
    do perform display.set_light(shown_5) as Any
    do sleep 350
    let shown_6 = {name: "red", state: "off"}
    do perform display.set_light(shown_6) as Any
    let shown_7 = {name: "amber", state: "on"}
    do perform display.set_light(shown_7) as Any
    do sleep 350
    let shown_8 = {name: "amber", state: "off"}
    do perform display.set_light(shown_8) as Any
    let shown_9 = {name: "green", state: "on"}
    do perform display.set_light(shown_9) as Any
    let shown_10 = {text: "Go"}
    do perform display.show_message(shown_10) as Any
    do sleep 500
  }
  let shown_11 = {key: "traffic", value: "complete"}
  do perform display.set_status(shown_11) as Any
  return null
}

main {
  finish null
}
"#;
