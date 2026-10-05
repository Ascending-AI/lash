//! A run states its own tool grants (FIG-5093).
//!
//! A spec's `tool_access` override is the run's tool authority, whole, in
//! place of the session's for that run only. It is resolved once with the
//! run's shape and recorded, so a toolbox switch or a second sender is an
//! accepted input that never waits behind the running turn, every replay
//! and cold reopen of the run sees the grants it recorded, and the sticky
//! session tool access a config command sets keeps shaping every run that
//! states none.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{RunOutcome, ShiftOutcome};
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

use super::run_spec_shift::{CrashBeforeModelCall, committed_runs, enqueue, head_config, shift};
use super::shift_admission::{ShiftParts, on_tier};
use crate::admit;

/// The tool only an alpha grant names.
const ALPHA: &str = "run_grant_alpha";
/// The tool only a beta grant names.
const BETA: &str = "run_grant_beta";

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool schemas and admission checks their invariant"
)]
fn grant_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A tool a run is granted or not.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
    .expect("valid declared tool schemas")
}

/// Both law tools, from one provider the session registers.
struct GrantTools;

#[async_trait::async_trait]
impl crate::ToolProvider for GrantTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        [ALPHA, BETA]
            .into_iter()
            .map(|name| grant_tool(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        [ALPHA, BETA]
            .contains(&name)
            .then(|| Arc::new(grant_tool(name).contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({})),
        ))
    }
}

/// A run granted exactly the law tools `names`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's distinct tools form a valid grant"
)]
fn granted(names: &[&str]) -> crate::SessionToolAccess {
    crate::SessionToolAccess::restricted(names.iter().map(|name| grant_tool(name)))
        .expect("distinct law tools form a grant")
}

/// A spec stating the grant `names` for its run.
fn grant_spec(names: &[&str]) -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        tool_access: Some(granted(names)),
        ..crate::RunOverrides::default()
    })
}

type SeenTools = Arc<std::sync::Mutex<Vec<Vec<String>>>>;

/// Install the law tools and serve every model call through a provider that
/// records the law tools each call was offered, sorted, and answers with
/// text. `on_call` runs inside each call before it answers, with its index.
fn record_tools(
    parts: &mut ShiftParts,
    on_call: impl Fn(usize) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> SeenTools {
    parts
        .plugins
        .push(Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("conformance-run-grant-tools"),
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(GrantTools)),
        )));
    let seen = SeenTools::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let on_call = Arc::new(on_call);
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let seen = Arc::clone(&seen);
            move |request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let mut offered = request
                    .tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .filter(|name| [ALPHA, BETA].contains(&name.as_str()))
                    .collect::<Vec<_>>();
                offered.sort();
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(offered);
                let during = on_call(index);
                async move {
                    during.await;
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: format!("answer {}", index + 1),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.models =
        crate::testing::standard_test_llm_profiles(provider.into_handle());
    seen
}

fn seen(tools: &SeenTools) -> Vec<Vec<String>> {
    tools
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

/// Two runs stating different grants on one session, accepted back to
/// back, each run under their own grants alone, and a run that states none
/// under the session's. The second and third inputs are accepted while the
/// first run's model call is in flight: stating grants is an acceptance,
/// never a config write that waits for the running turn. The sticky config
/// never takes a run's grants.
pub async fn runs_stating_different_tool_grants_each_run_under_their_own(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-grants-own", &effect_host, &stores, 8).await;
    let sender = parts.clone();
    let accepted_during_first = Arc::new(AtomicUsize::new(0));
    let tools = record_tools(&mut parts, {
        let accepted = Arc::clone(&accepted_during_first);
        move |index| {
            let sender = sender.clone();
            let accepted = Arc::clone(&accepted);
            Box::pin(async move {
                if index == 0 {
                    enqueue(&sender, "beta only", "grants-beta", grant_spec(&[BETA])).await;
                    enqueue(
                        &sender,
                        "the session's tools",
                        "grants-default",
                        crate::RunSpec::default(),
                    )
                    .await;
                    accepted.store(2, Ordering::SeqCst);
                }
            })
        }
    });
    enqueue(&parts, "alpha only", "grants-alpha", grant_spec(&[ALPHA])).await;
    let outcome = shift(&runner, &parts, "run-grants-own-shift").await;
    assert_eq!(
        accepted_during_first.load(Ordering::SeqCst),
        2,
        "both later inputs were accepted while the first run's model call ran"
    );
    assert_eq!(
        committed_runs(&outcome),
        vec!["grants-alpha", "grants-beta", "grants-default"],
        "the shift runs the three runs back to back, in admission order"
    );
    assert_eq!(
        seen(&tools),
        vec![names(&[ALPHA]), names(&[BETA]), names(&[ALPHA, BETA])],
        "each run saw its own grants alone, and the run stating none the session's"
    );
    assert_eq!(
        head_config(&parts).await.tool_access,
        crate::SessionToolAccess::ambient(),
        "no run's grants reached the sticky config"
    );
}

/// A definition that grants `tools` to the runs naming it.
struct GrantingDefinition {
    tools: &'static [&'static str],
    resolved: Arc<AtomicUsize>,
}

const GRANTING_DEFINITION: &str = "run-grants-definition";

impl crate::RunDefinition for GrantingDefinition {
    fn reference(&self) -> crate::DefinitionRef {
        crate::DefinitionRef::new(GRANTING_DEFINITION, 1)
    }

    fn resolve(
        &self,
        _snapshot: &crate::PersistedSessionConfig,
        _context: &serde_json::Value,
    ) -> Result<crate::RunOverrides, crate::RunDefinitionRefusal> {
        self.resolved.fetch_add(1, Ordering::SeqCst);
        Ok(crate::RunOverrides {
            tool_access: Some(granted(self.tools)),
            ..crate::RunOverrides::default()
        })
    }
}

fn granting(tools: &'static [&'static str], resolved: &Arc<AtomicUsize>) -> crate::RunDefinitions {
    let mut definitions = crate::RunDefinitions::default();
    definitions.register(Arc::new(GrantingDefinition {
        tools,
        resolved: Arc::clone(resolved),
    }));
    definitions
}

/// A run's grants survive its crash and a cold reopen: the run crashes
/// after recording its shape and before its model call, and a fresh
/// runtime, whose deployment's definition would grant other tools, redrives
/// it. The redrive reads the recorded grants back and never resolves again,
/// so the run's one model call is offered the tools its first execution
/// granted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_runs_tool_grants_survive_its_crash_and_a_cold_reopen(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-grants-reopen", &effect_host, &stores, 8).await;
    let tools = record_tools(&mut parts, |_| Box::pin(async {}));
    let first_resolutions = Arc::new(AtomicUsize::new(0));
    let redrive_resolutions = Arc::new(AtomicUsize::new(0));
    let input = enqueue(
        &parts,
        "granted by the definition",
        "grants-reopen",
        crate::RunSpec::definition(
            crate::DefinitionRef::new(GRANTING_DEFINITION, 1),
            serde_json::Value::Null,
        ),
    )
    .await;
    let mut first = parts.clone();
    first.host.providers.run_definitions = granting(&[ALPHA], &first_resolutions);
    let mut redeployed = parts.clone();
    redeployed.host.providers.run_definitions = granting(&[BETA], &redrive_resolutions);
    let request = parts.request("run-grants-reopen-shift");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ShiftOutcome>();
    let attempt = |parts: ShiftParts, crash: bool| -> crate::ConformanceTurnAttempt {
        let request = request.clone();
        let tx = tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                // Every attempt opens the session cold from the store.
                let mut runtime = parts.runtime().await;
                if crash {
                    runtime.set_turn_phase_probe(Arc::new(CrashBeforeModelCall));
                }
                let outcome = lash_core::shift::work_session(&mut runtime, &scope, &request)
                    .await
                    .expect("the redriven shift runs");
                assert!(!crash, "the crash fires before the run's model call");
                let _ = tx.send(outcome);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("shift-law-driver"),
            )),
            attempt(first, true),
            attempt(redeployed, false),
        )
        .await;
    let outcome = rx.recv().await.expect("the redrive ran the shift");
    assert_eq!(committed_runs(&outcome), vec!["grants-reopen"]);
    assert_eq!(
        (
            first_resolutions.load(Ordering::SeqCst),
            redrive_resolutions.load(Ordering::SeqCst),
        ),
        (1, 0),
        "the first execution resolved the grants once; the redrive read them back"
    );
    assert_eq!(
        seen(&tools),
        vec![names(&[ALPHA])],
        "the run's one model call was offered the grants it recorded"
    );
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("grants-reopen"))],
        "the run commits once"
    );
}

/// A session tool-access command still shapes every later run that states
/// no grants of its own. The command is pending ahead of a default input,
/// so the command lane applies it first and that run runs under it; a run
/// stating grants then runs under its grants alone, and the default run
/// after it is back on the commanded access, which the head keeps.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_tool_access_command_still_shapes_later_runs_that_state_no_grants(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-grants-command", &effect_host, &stores, 8).await;
    let tools = record_tools(&mut parts, |_| Box::pin(async {}));
    let commanded = crate::SessionToolAccess::ambient()
        .with_hidden_tools([BETA])
        .expect("one hidden law tool");
    enqueue(
        &parts,
        "before the command",
        "command-before",
        crate::RunSpec::default(),
    )
    .await;
    let request = parts.request("run-grants-command-shift");
    let access = commanded.clone();
    let first: ShiftOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        let access = access.clone();
        Box::pin(async move {
            let revision = runtime.config_revision();
            runtime
                .submit_config_transaction(
                    "run-grants-command",
                    revision,
                    &crate::ConfigTransaction::of(crate::plugin::config::core::SetToolAccess {
                        access,
                    }),
                )
                .await
                .expect("the config command is accepted");
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs")
        })
    })
    .await;
    assert!(
        matches!(first.ran.first(), Some(RunOutcome::Applied { .. })),
        "the command lane applies first: {first:?}"
    );
    assert_eq!(committed_runs(&first), vec!["command-before"]);

    enqueue(&parts, "beta only", "command-granted", grant_spec(&[BETA])).await;
    enqueue(
        &parts,
        "after the grant",
        "command-after",
        crate::RunSpec::default(),
    )
    .await;
    let second = shift(&runner, &parts, "run-grants-command-shift-2").await;
    assert_eq!(
        committed_runs(&second),
        vec!["command-granted", "command-after"]
    );
    assert_eq!(
        seen(&tools),
        vec![names(&[ALPHA]), names(&[BETA]), names(&[ALPHA])],
        "runs stating no grants ran under the command's access; the granted run under its own"
    );
    assert_eq!(
        head_config(&parts).await.tool_access,
        commanded,
        "the head keeps the command's access, never the run's grants"
    );
}
