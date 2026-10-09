//! The built-in workflows, written as kernel documents.

use lash::workflow::WorkflowEnvironment;
use lash::workflow::document::{Document, Name, parse_document};
use serde::{Deserialize, Serialize};

use crate::runtime::{RunError, complete};

mod documents;

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

/// A built-in workflow. Its text is a kernel document whose one entry is
/// the workflow; `@{name}` stands for the identity this host's environment
/// gives the library function `name`.
struct BuiltInWorkflow {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    entry: &'static str,
    text: &'static str,
}

/// The workflow the example opens with.
pub(crate) const DEFAULT_WORKFLOW: &str = "onboarding";

const BUILT_IN_WORKFLOWS: &[BuiltInWorkflow] = &[
    BuiltInWorkflow {
        id: "blank",
        name: "Blank workflow",
        description: "An empty starter you build up with edits.",
        entry: "blank",
        text: documents::BLANK,
    },
    BuiltInWorkflow {
        id: "onboarding",
        name: "Onboarding",
        description: "An onboarding flow with a host approval call, a branch, a loop and mixed display updates.",
        entry: "onboarding",
        text: documents::ONBOARDING,
    },
    BuiltInWorkflow {
        id: "summarize-emails",
        name: "Summarize my top 5 emails",
        description: "List five recent emails, summarize each one, and show the digest.",
        entry: "summarize_top_emails",
        text: documents::SUMMARIZE_TOP_EMAILS,
    },
    BuiltInWorkflow {
        id: "research-nvidia-stock",
        name: "Research NVIDIA stock",
        description: "Research NVIDIA's stock outlook and show a concise mocked briefing.",
        entry: "research_nvidia_stock",
        text: documents::RESEARCH_NVIDIA_STOCK,
    },
    BuiltInWorkflow {
        id: "team-standup-digest",
        name: "Team standup digest",
        description: "Collect Slack and GitHub activity, then present a daily team brief.",
        entry: "team_standup_digest",
        text: documents::TEAM_STANDUP_DIGEST,
    },
    BuiltInWorkflow {
        id: "traffic-lights",
        name: "Traffic Lights",
        description: "A visual red, amber, and green light sequence repeated twice.",
        entry: "traffic_lights",
        text: documents::TRAFFIC_LIGHTS,
    },
    BuiltInWorkflow {
        id: "branching-approval",
        name: "Branching Approval",
        description: "An if-heavy approval flow with a visible host approval call and distinct outcomes.",
        entry: "branching_approval",
        text: documents::BRANCHING_APPROVAL,
    },
    BuiltInWorkflow {
        id: "counter-loop",
        name: "Counter Loop",
        description: "A while loop followed by a for loop, with progress updates.",
        entry: "counter_loop",
        text: documents::COUNTER_LOOP,
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

/// The built-in workflow `id` as a document `environment` admits, with the
/// entry a run of it starts; `None` when the catalog has no such workflow.
pub(crate) fn document(
    id: &str,
    environment: &WorkflowEnvironment,
) -> Option<Result<(Document, Name), RunError>> {
    let workflow = BUILT_IN_WORKFLOWS
        .iter()
        .find(|workflow| workflow.id == id)?;
    let mut text = workflow.text.to_owned();
    for (function, registered) in environment.functions().iter() {
        text = text.replace(
            &format!("@{{{}}}", registered.definition.name),
            &format!("@{function}"),
        );
    }
    Some(
        parse_document(&text)
            .map_err(|error| RunError::Invalid(format!("built-in workflow `{id}`: {error:?}")))
            .and_then(|document| complete(document, environment))
            .map(|document| (document, Name::new(workflow.entry))),
    )
}
