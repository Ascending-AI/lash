use serde::{Deserialize, Serialize};

use crate::DEFAULT_WORKFLOW;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SelectWorkflowRequest {
    pub id: String,
}

pub(crate) struct BuiltInWorkflow {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub source: &'static str,
}

// Every catalog corpus is TypeScript: it is the only cell language, and the
// lens's canonical text is TypeScript (FIG-3033). Authored names are spelled
// as `@label` doc comments (FIG-3047), which is what an editor rename writes
// back into source; the corpora that carry them are the fixture proving
// render -> parse -> render is a fixed point over a labeled program. `blank`
// and the tool-facing examples carry none, so the derived-name path stays
// exercised too.

const BLANK_WORKFLOW: &str = r#"const blank = async () => {
  return 0;
};
"#;

const TRAFFIC_LIGHTS_WORKFLOW: &str = r#"/** @label Traffic lights — Cycle a three-light signal twice */
const traffic_lights = async () => {
  await display.set_status({ key: "traffic", value: "running" });
  /** @label Run two cycles */
  for (const cycle of [1, 2]) {
    await display.add_item({ list: "cycles", item: cycle });
    /** @label Red — Stop the traffic */
    await display.set_light({ name: "red", state: "on" });
    await display.set_light({ name: "amber", state: "off" });
    await display.set_light({ name: "green", state: "off" });
    await sleep("350ms");
    await display.set_light({ name: "red", state: "off" });
    await display.set_light({ name: "amber", state: "on" });
    await sleep("350ms");
    await display.set_light({ name: "amber", state: "off" });
    await display.set_light({ name: "green", state: "on" });
    await display.show_message({ text: "Go" });
    await sleep("500ms");
  }
  await display.set_status({ key: "traffic", value: "complete" });
  return null;
};
"#;

const BRANCHING_APPROVAL_WORKFLOW: &str = r#"/** @label Branching approval — Wait for a decision and take one of two paths */
const branching_approval = async () => {
  await display.set_status({ key: "approval", value: "waiting" });
  await display.highlight({ target: "approval" });
  await display.show_message({ text: "Approval requested" });
  /** @label Wait for the decision */
  const decision = await waitSignal("continue");
  if (decision.autoFired) {
    await display.set_status({ key: "approval", value: "approved" });
    if (true) {
      await display.set_light({ name: "approved", state: "green" });
      await display.show_message({ text: "Request approved" });
    } else {
      await display.show_message({ text: "Approval needs review" });
    }
  } else {
    await display.set_status({ key: "approval", value: "rejected" });
    await display.set_light({ name: "rejected", state: "red" });
    await display.show_message({ text: "Request rejected" });
  }
  await sleep("400ms");
  await display.highlight({ target: "result" });
  return decision;
};
"#;

const COUNTER_LOOP_WORKFLOW: &str = r#"/** @label Counter loop — Count to three, then walk a fixed progress list */
const counter_loop = async () => {
  await display.set_status({ key: "counter", value: "running" });
  await display.set_progress({ pct: 5 });
  const state = { count: 0 };
  /** @label Count to three */
  while (state.count < 3) {
    await display.add_item({ list: "counts", item: state.count });
    await display.set_progress({ pct: state.count * 20 + 20 });
    state.count = state.count + 1;
    await sleep("250ms");
  }
  /** @label Walk the progress list */
  for (const pct of [70, 85, 100]) {
    await display.set_progress({ pct: pct });
    await sleep("300ms");
  }
  await display.highlight({ target: "progress" });
  await display.set_status({ key: "counter", value: "complete" });
  await display.show_message({ text: "Counter complete" });
  return state.count;
};
"#;

const SUMMARIZE_EMAILS_WORKFLOW: &str = r#"const summarize_top_emails = async () => {
  const emails = await gmail.list_recent({ count: 5 });
  for (const email of emails) {
    await llm.query({
      task: "Summarize this email in one sentence",
      inputs: { sender: email["from"], subject: email.subject, snippet: email.snippet }
    });
  }
  const digest = await llm.query({
    task: "Format these five summaries as a concise numbered email digest",
    inputs: { summaries: emails }
  });
  await display.show_message({ text: digest });
  return digest;
};
"#;

const RESEARCH_NVIDIA_WORKFLOW: &str = r#"const research_nvidia_stock = async () => {
  const search = await web.search({ query: "NVIDIA stock outlook" });
  const research = await agents.spawn({
    capability: "explore",
    task: "Research NVIDIA's stock outlook from the supplied web search results",
    seed: { search_results: search.results }
  });
  await display.show_message({ text: research.summary });
  return research;
};
"#;

const TEAM_STANDUP_WORKFLOW: &str = r#"const team_standup_digest = async () => {
  const messages = await slack.recent({ channel: "team-platform", since: "yesterday" });
  const activity = await github.recent({ repo: "acme/widgets", since: "yesterday" });
  const standup = await agents.spawn({
    capability: "peer",
    task: "Synthesize a concise team standup digest and call out blockers",
    seed: { slack_messages: messages, github_activity: activity }
  });
  await display.show_message({ text: standup.digest });
  return standup.digest;
};
"#;

pub(crate) const BUILT_IN_WORKFLOWS: &[BuiltInWorkflow] = &[
    BuiltInWorkflow {
        id: "blank",
        name: "Blank workflow",
        description: "An empty starter you build up by adding nodes.",
        source: BLANK_WORKFLOW,
    },
    BuiltInWorkflow {
        id: "onboarding",
        name: "Onboarding",
        description: "A labeled onboarding flow with a signal wait, branch, and mixed display updates.",
        source: DEFAULT_WORKFLOW,
    },
    BuiltInWorkflow {
        id: "summarize-emails",
        name: "Summarize my top 5 emails",
        description: "List five recent emails, summarize each one, and show the digest.",
        source: SUMMARIZE_EMAILS_WORKFLOW,
    },
    BuiltInWorkflow {
        id: "research-nvidia-stock",
        name: "Research NVIDIA stock",
        description: "Research NVIDIA's stock outlook and show a concise mocked briefing.",
        source: RESEARCH_NVIDIA_WORKFLOW,
    },
    BuiltInWorkflow {
        id: "team-standup-digest",
        name: "Team standup digest",
        description: "Collect Slack and GitHub activity, then present a daily team brief.",
        source: TEAM_STANDUP_WORKFLOW,
    },
    BuiltInWorkflow {
        id: "traffic-lights",
        name: "Traffic Lights",
        description: "A visual red, amber, and green light sequence repeated twice.",
        source: TRAFFIC_LIGHTS_WORKFLOW,
    },
    BuiltInWorkflow {
        id: "branching-approval",
        name: "Branching Approval",
        description: "An if-heavy approval flow with a visible signal wait and distinct outcomes.",
        source: BRANCHING_APPROVAL_WORKFLOW,
    },
    BuiltInWorkflow {
        id: "counter-loop",
        name: "Counter Loop",
        description: "A structured while loop followed by an editable for container and progress updates.",
        source: COUNTER_LOOP_WORKFLOW,
    },
];

pub(crate) fn entries() -> Vec<WorkflowCatalogEntry> {
    BUILT_IN_WORKFLOWS
        .iter()
        .map(|workflow| WorkflowCatalogEntry {
            id: workflow.id.to_string(),
            name: workflow.name.to_string(),
            description: workflow.description.to_string(),
        })
        .collect()
}

pub(crate) fn source(id: &str) -> Option<&'static str> {
    BUILT_IN_WORKFLOWS
        .iter()
        .find(|workflow| workflow.id == id)
        .map(|workflow| workflow.source)
}
