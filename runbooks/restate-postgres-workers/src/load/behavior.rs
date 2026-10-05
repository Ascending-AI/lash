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
    /// The immutable definition the registry record was admitted from.
    pub record_definition: String,
    /// The definition the stored module artifact derives for the record's
    /// process ref, empty when the artifact exports no such process.
    pub artifact_definition: String,
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

/// The promotion readback of one started process: its registry record and the
/// immutable module artifact its persisted engine input names. A process is
/// identified by its definition id, never by a name (ADR 0095, ADR 0107 §1);
/// the persisted input carries no process name.
pub(crate) fn promotion(
    process_id: String,
    session_origin: bool,
    engine: String,
    record: &lash::process::ProcessIdentity,
    input: &lash::process::LashlangProcessInput,
    artifact: &lash::rlm::lang::ModuleArtifact,
) -> PromotionEvidence {
    let artifact_definition = artifact
        .process_name_for_ref(&input.process_ref)
        .and_then(|_| {
            lash::rlm::lang::ProcessDefinitionIdentity::new(
                artifact.module_ref().clone(),
                artifact.host_requirements_ref().clone(),
                input.process_ref.clone(),
                String::new(),
            )
            .draft()
            .ok()
        })
        .map(|draft| draft.id().to_string())
        .unwrap_or_default();
    PromotionEvidence {
        process_id,
        session_origin,
        engine,
        record_definition: record
            .definition_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        artifact_definition,
    }
}

fn seed(text: &str) -> Vec<lash::plugins::SessionAppendNode> {
    vec![lash::plugins::SessionAppendNode::message(
        lash::plugins::PluginMessage::text(lash::messages::MessageRole::Assistant, text),
    )]
}

/// Host policy for the smoke only. Core owns both durable frame writes.
pub struct LoadCompaction;
#[async_trait::async_trait]
impl lash::plugins::ContextCompactor for LoadCompaction {
    fn id(&self) -> &'static str {
        "load.compaction"
    }
    async fn compact(
        &self,
        ctx: &lash::plugins::CompactionContext<'_>,
    ) -> Result<Option<lash::plugins::ContextCompaction>, lash::plugins::ContextError> {
        Ok(ctx
            .session_id
            .as_str()
            .ends_with("-behaviors")
            .then(|| lash::plugins::ContextCompaction::new(seed(ADMIN_SUMMARY))))
    }
}
#[async_trait::async_trait]
impl lash::plugins::ContextPressureHook for LoadCompaction {
    fn id(&self) -> &'static str {
        "load.pressure"
    }
    async fn decide(
        &self,
        ctx: &lash::plugins::ContextPressureContext<'_>,
    ) -> Result<lash::plugins::ContextPressureDecision, lash::plugins::ContextError> {
        if ctx.session_id.as_str().ends_with("-behaviors")
            && ctx
                .prompt_usage
                .as_ref()
                .is_some_and(|usage| usage.input_tokens >= 100_000)
        {
            Ok(lash::plugins::ContextPressureDecision::OpenFrame {
                records: vec![],
                task: "load context pressure".into(),
                seed: seed(PRESSURE_SUMMARY),
            })
        } else {
            Ok(lash::plugins::ContextPressureDecision::Continue)
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(crate) fn register(
    reg: &mut lash::plugins::PluginRegistrar,
) -> Result<(), lash::plugins::PluginError> {
    reg.turn().after(lash::hook_key!("load-behavior"), Arc::new(|ctx:lash::plugins::TurnResultHookContext| Box::pin(async move {
        if !ctx.turn.errors.is_empty() {
            tracing::error!(session_id=%ctx.session_id, errors=?ctx.turn.errors, outcome=?ctx.turn.outcome,"load turn failed");
        }
        Ok(Default::default())
    })))?;
    reg.context().compact(100, Arc::new(LoadCompaction))?;
    reg.context().pressure(100, Arc::new(LoadCompaction))?;
    reg.triggers().declare(lash::triggers::TriggerEvent::new("Event", "load.external", "event", lash::schema::JsonSchema::admit(json!({"type":"object","properties":{"schedule":{"type":"string"},"tick":{"type":"string"}},"required":["schedule","tick"],"additionalProperties":false})).expect("valid declared payload schema")))?;
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
