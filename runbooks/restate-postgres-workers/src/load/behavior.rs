//! Bounded FIG-4241 operations and their independently checked readbacks.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub const MARKER: &str = "load_behavior=";
pub const EXTERNAL_SOURCE: &str = "load.external.event";
pub const ADMIN_SUMMARY: &str = "load administrative summary";
pub const PRESSURE_SUMMARY: &str = "load pressure summary";
pub const AUXILIARY_ANSWER: &str = "auxiliary answer";
pub const CLASSES: [&str; 8] = [
    "provider-streams",
    "history-prefill",
    "admin-compaction",
    "context-pressure",
    "auxiliary-requests",
    "external-occurrences",
    "trigger-edits",
    "promotion-reads",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEvidence {
    pub expected: Vec<String>,
    pub reopened: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameEvidence {
    pub before: String,
    pub after: String,
    pub summary: String,
    pub applied: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuxiliaryEvidence {
    pub key: String,
    pub output: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OccurrenceEvidence {
    pub key: String,
    pub started: Vec<String>,
    pub outputs: Vec<Value>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditEvidence {
    pub revision: u64,
    pub listed_revision: u64,
    pub started: Vec<String>,
    pub outputs: Vec<Value>,
    pub after_delete: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromotionEvidence {
    pub process_id: String,
    pub session_origin: bool,
    pub engine: String,
    pub record_name: String,
    pub artifact_name: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BehaviorReport {
    pub prefill: HistoryEvidence,
    pub admin: FrameEvidence,
    pub pressure: FrameEvidence,
    pub auxiliary: AuxiliaryEvidence,
    pub external: OccurrenceEvidence,
    pub edit: EditEvidence,
    pub promotion: PromotionEvidence,
}

fn seed(text: &str) -> Vec<lash_core::SessionAppendNode> {
    vec![lash_core::SessionAppendNode::message(
        lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, text),
    )]
}

/// Host policy for the smoke only. Core owns both durable frame writes.
pub struct LoadCompaction;
#[async_trait::async_trait]
impl lash_core::facade_support::ContextCompactor for LoadCompaction {
    fn id(&self) -> &'static str {
        "load.compaction"
    }
    async fn compact(
        &self,
        ctx: &lash_core::facade_support::CompactionContext<'_>,
    ) -> Result<
        Option<lash_core::facade_support::ContextCompaction>,
        lash_core::facade_support::ContextError,
    > {
        Ok(ctx
            .session_id
            .as_str()
            .ends_with("-behaviors")
            .then(|| lash_core::facade_support::ContextCompaction::new(seed(ADMIN_SUMMARY))))
    }
}
#[async_trait::async_trait]
impl lash_core::facade_support::ContextPressureHook for LoadCompaction {
    fn id(&self) -> &'static str {
        "load.pressure"
    }
    async fn decide(
        &self,
        ctx: &lash_core::facade_support::ContextPressureContext<'_>,
    ) -> Result<
        lash_core::facade_support::ContextPressureDecision,
        lash_core::facade_support::ContextError,
    > {
        if ctx.session_id.as_str().ends_with("-behaviors")
            && ctx
                .prompt_usage
                .as_ref()
                .is_some_and(|usage| usage.input_tokens >= 100_000)
        {
            Ok(
                lash_core::facade_support::ContextPressureDecision::OpenFrame {
                    records: vec![],
                    task: "load context pressure".into(),
                    seed: seed(PRESSURE_SUMMARY),
                },
            )
        } else {
            Ok(lash_core::facade_support::ContextPressureDecision::Continue)
        }
    }
}

pub(crate) fn register(
    reg: &mut lash_core::plugin::PluginRegistrar,
) -> Result<(), lash_core::PluginError> {
    reg.turn().after(Arc::new(|ctx:lash_core::plugin::TurnResultHookContext| Box::pin(async move {
        if !ctx.turn.errors.is_empty() {
            tracing::error!(session_id=%ctx.session_id, errors=?ctx.turn.errors, outcome=?ctx.turn.outcome,"load turn failed");
        }
        Ok(vec![])
    })));
    reg.context().compact(100, Arc::new(LoadCompaction));
    reg.context().pressure(100, Arc::new(LoadCompaction));
    reg.triggers().declare(lash::triggers::TriggerEvent::new("Event", "load.external", "event", lash::triggers::LashSchema::new(json!({"type":"object","properties":{"schedule":{"type":"string"},"tick":{"type":"string"}},"required":["schedule","tick"],"additionalProperties":false}))))?;
    Ok(())
}

pub(crate) fn register_source(catalog: &mut lash::rlm::LashlangHostCatalog) -> Result<(), String> {
    let event = lash::rlm::NamedDataType::object(
        "load.external.Event",
        vec![
            lash::rlm::TypeField {
                name: "schedule".into(),
                ty: lash::rlm::TypeExpr::Str,
                optional: false,
            },
            lash::rlm::TypeField {
                name: "tick".into(),
                ty: lash::rlm::TypeExpr::Str,
                optional: false,
            },
        ],
    )
    .map_err(|e| e.to_string())?;
    catalog
        .add_trigger_source_constructor(
            ["load", "external", "event"],
            lash::rlm::TypeExpr::Object(vec![lash::rlm::TypeField {
                name: "schedule".into(),
                ty: lash::rlm::TypeExpr::Str,
                optional: false,
            }]),
            event,
        )
        .map_err(|e| e.to_string())
}

pub fn script(run: &str, phase: &str) -> anyhow::Result<String> {
    let schedule = serde_json::to_string(&format!("{run}/behaviors"))?;
    let body = match phase {
        "auxiliary" => format!("const answer=await llm.query({{task:{}, inputs:{{}}}});finish({{answer:answer}});",serde_json::to_string(&format!("{MARKER}{run}/llm"))?),
        "register" => format!("const on_event=async(event:load.external.Event)=>{{await tools.mark({{key:event.tick}});return {{key:event.tick,revision:1}};}};const registered=await triggers.register({{source:load.external.event({{schedule:{schedule}}}),target:{{definition:on_event}},inputs:(event)=>({{event:event}}),subscription_key:\"load-external\",name:\"external\"}});finish({{revision:registered.revision}});"),
        "edit" => format!("const on_event=async(event:load.external.Event)=>{{await tools.mark({{key:event.tick}});return {{key:event.tick,revision:2}};}};const edited=await triggers.update({{subscription_key:\"load-external\",expected_revision:1,source:load.external.event({{schedule:{schedule}}}),target:{{definition:on_event}},inputs:(event)=>({{event:event}}),name:\"edited\"}});finish({{revision:edited.revision}});"),
        "delete" => "const deleted=await triggers.delete({subscription_key:\"load-external\",expected_revision:2});finish({deleted:deleted});".into(),
        "pressure" | "pressure_usage" | "seed" => "finish({synthetic:true});".into(),
        _ => anyhow::bail!("unknown load behavior {phase}"),
    };
    Ok(format!("<typescript>\n{body}\n</typescript>"))
}

pub fn prefill(load: &super::LoadContext, run: &str) -> anyhow::Result<Vec<String>> {
    let generator = load.generator(run)?;
    let count = generator.prefill_turns(0, 0);
    Ok((0..count * 2)
        .map(|index| generator.text(0, 0, &format!("prefill/{index}"), 96))
        .collect())
}
