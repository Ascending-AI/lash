//! Compiled sources for the Rust snippets on `docs/embedding-turns.html`.

use lash::{FrameKey, TurnFinish, TurnOutcome, TurnStop};
use lash::{LashCore, LashSession, TurnInput, TurnReport};

fn persist_terminal(_finish: TurnFinish) -> anyhow::Result<()> {
    Ok(())
}

fn record_frame_boundary(_frame_key: FrameKey) -> anyhow::Result<()> {
    Ok(())
}

fn report_user_visible(_stop: TurnStop) {}

fn offer_retry(_stop: TurnStop) {}

/// `MaxTurns` has two causes, so the host's response is to read which bound
/// was hit rather than to assume the turn budget. The `no_progress_budget`
/// diagnostic on the turn's events names a no-progress stop; without it the
/// turn budget is the bound that ran out.
fn review_turn_bounds() {}

fn record_for_diagnosis(_stop: TurnStop) {}

fn outcome_match(result: TurnReport) -> anyhow::Result<()> {
    // docs:start:outcome-match
    match result.outcome {
        TurnOutcome::Finished(finish) => persist_terminal(finish)?,
        TurnOutcome::AgentFrameSwitch { frame_key, .. } => record_frame_boundary(frame_key)?,
        TurnOutcome::Stopped(stop) => match stop {
            TurnStop::Cancelled { .. } | TurnStop::InvalidInput => report_user_visible(stop),
            TurnStop::ProviderError | TurnStop::Incomplete => offer_retry(stop),
            // Either bound: the turn budget, or the no-progress budget when
            // consecutive attempts committed no successful execution.
            TurnStop::MaxTurns => review_turn_bounds(),
            other => record_for_diagnosis(other),
        },
    }
    // docs:end:outcome-match
    Ok(())
}

type AppUiTx = tokio::sync::mpsc::Sender<String>;

/// Opaque row handle in the host UI; cheap to clone, not Copy.
#[derive(Clone)]
struct UiRowId;

async fn append_live_text(_text: String) {}

async fn upsert_reasoning_row(_row: Option<UiRowId>, _text: String) -> UiRowId {
    UiRowId
}

async fn insert_tool_row(_name: String, _args: serde_json::Value) -> UiRowId {
    UiRowId
}

async fn update_or_insert_tool_row(
    _row: Option<UiRowId>,
    _name: String,
    _output: lash::tools::ToolCallOutput,
) {
}

async fn insert_code_row(_language: String, _code: String) -> UiRowId {
    UiRowId
}

async fn update_or_insert_code_row(
    _row: Option<UiRowId>,
    _language: String,
    _output: String,
    _error: Option<String>,
    _success: bool,
) {
}

async fn record_terminal_tool(_tool_name: String) {}

async fn update_usage(_usage: lash::usage::TokenUsage, _cumulative: lash::usage::TokenUsage) {}

async fn update_child_usage(
    _source: String,
    _usage: lash::usage::TokenUsage,
    _cumulative: lash::usage::TokenUsage,
) {
}

// docs:start:ui-sink
use async_trait::async_trait;
use lash::sync::MutexExt;
use lash::{TurnActivity, TurnActivitySink, TurnEvent};

struct AppEvents {
    tx: AppUiTx,
    turn_state: std::sync::Mutex<TurnUiState>,
}

#[derive(Default)]
struct TurnUiState {
    reasoning: Option<UiRowId>,
    tools: std::collections::HashMap<String, UiRowId>,
    code: Option<UiRowId>,
}

#[async_trait]
impl TurnActivitySink for AppEvents {
    async fn emit(&self, activity: TurnActivity) {
        let correlation_id = activity.correlation_id.0.to_string();
        match activity.event {
            TurnEvent::AssistantProseDelta { text } => {
                append_live_text(text.to_string()).await;
            }
            TurnEvent::ReasoningDelta { text } => {
                let row = self.turn_state.lock_recover().reasoning.clone();
                let row = upsert_reasoning_row(row, text.to_string()).await;
                self.turn_state.lock_recover().reasoning = Some(row);
            }
            TurnEvent::ToolCallStarted { name, args, .. } => {
                let row = insert_tool_row(name, args).await;
                self.turn_state
                    .lock_recover()
                    .tools
                    .insert(correlation_id, row);
            }
            TurnEvent::ToolCallCompleted { name, output, .. } => {
                let row = self.turn_state.lock_recover().tools.remove(&correlation_id);
                update_or_insert_tool_row(row, name, output).await;
            }
            TurnEvent::CodeBlockStarted { language, code, .. } => {
                let row = insert_code_row(language, code).await;
                self.turn_state.lock_recover().code = Some(row);
            }
            TurnEvent::CodeBlockCompleted {
                language,
                output,
                error,
                success,
                ..
            } => {
                let row = self.turn_state.lock_recover().code.take();
                update_or_insert_code_row(row, language, output, error, success).await;
            }
            TurnEvent::FinalValue { value } => {
                append_live_text(render_terminal_value(&value)).await;
            }
            TurnEvent::ToolValue { tool_name, value } => {
                append_live_text(render_terminal_value(&value)).await;
                record_terminal_tool(tool_name).await;
            }
            TurnEvent::Usage {
                usage, cumulative, ..
            } => {
                update_usage(usage, cumulative).await;
            }
            TurnEvent::ChildUsage {
                source,
                usage,
                cumulative,
                ..
            } => {
                update_child_usage(source, usage, cumulative).await;
            }
            _ => {}
        }
    }
}

fn render_terminal_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}
// docs:end:ui-sink

// docs:start:channel-sink
use tokio::sync::mpsc;

struct ChannelSink {
    tx: mpsc::Sender<TurnActivity>,
}

#[async_trait::async_trait]
impl TurnActivitySink for ChannelSink {
    async fn emit(&self, activity: TurnActivity) {
        // send().await yields when the channel is full — the turn
        // will pause here if your UI consumer falls behind.
        let _ = self.tx.send(activity).await;
    }
}
// docs:end:channel-sink

async fn rlm_terminal_contracts(
    session: &LashSession,
    sink: lash::runtime::NoopTurnActivitySink,
) -> anyhow::Result<()> {
    // docs:start:rlm-terminal-contracts
    use lash::rlm::RlmTurnBuilderExt as _;

    let finished = session
        .turn(TurnInput::text("Move on the board."))
        .require_finish()?
        .stream_to(&sink)
        .await?;

    let natural = session
        .turn(TurnInput::text("Answer directly if no code is needed."))
        .allow_prose_or_finish()?
        .run()
        .await?;
    // docs:end:rlm-terminal-contracts
    Ok(())
}

async fn cancel_turn(core: &LashCore, session: &LashSession) -> anyhow::Result<()> {
    // docs:start:cancel-turn
    use lash::{
        TurnAddress, TurnCancelDisposition, TurnCancellationEvidence, TurnOutcome, TurnStop,
    };

    let turn_id = "incident-summary-42";
    let stream = session
        .turn(TurnInput::text("Summarize the incident."))
        .turn_id(turn_id)
        .stream()?;

    // An HTTP handler can retain only these routing ids. Authenticate and
    // authorize the caller before forwarding them to Lash. The default inline
    // driver is same-process; cross-process cancellation requires a durable
    // engine deployment. "user" is this host's vocabulary; Lash carries it
    // opaquely without assigning semantics. Choose Drop when undelivered input
    // should return to the host instead of being deferred to the next turn.
    let receipt = session
        .request_turn_cancel_with_disposition(
            turn_id,
            "stop-button-7",
            Some("user".to_string()),
            Some("operator pressed Stop".to_string()),
            TurnCancelDisposition::Drop,
        )
        .await?;
    let _receipt = receipt;

    let result = stream.finish().await?;
    // The evidence that settled the cancellation rides the outcome, so a
    // cancelled turn always names the request that stopped it. Match on the
    // variant, or read it with `TurnOutcome::cancellation`.
    if let TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) = &result.outcome {
        assert_eq!(evidence.request_id, "stop-button-7");
        assert_eq!(TurnOutcome::cancellation(&result.outcome), Some(evidence));
    }
    // Lash mints its own evidence when no host request explains the stop, so
    // the request id is namespaced rather than absent.
    assert_eq!(
        TurnCancellationEvidence::internal(turn_id).request_id,
        format!("internal:{turn_id}")
    );

    // Attachment is idempotent and returns immediately after publication.
    let terminal = core
        .turn_work_driver()
        .await_terminal_with_timeout(
            &session.turn_address(turn_id),
            std::time::Duration::from_secs(30),
        )
        .await?;
    // docs:end:cancel-turn
    Ok(())
}

fn restate_turn_control(ingress_url: &str) {
    // docs:start:restate-turn-control
    let deployment = lash_restate::RestateTurnDeployment::new(ingress_url);

    // Configure the core with this host. Bind LashDurableWaitWorkflowImpl and
    // LashDurableWaitIndexImpl on the Restate endpoint alongside turn handlers.
    let effect_host = deployment.effect_host();

    // This durable driver can live in a different web process from the turn
    // owner. It uses LashDurableWaitWorkflow—not the Restate Admin API—and
    // survives web-process restarts. Native turn work is same-process.
    let driver = deployment.turn_work_driver();
    let terminal_attach = deployment.turn_attach();
    // docs:end:restate-turn-control
}

fn persist_typed_value(_value: serde_json::Value) -> anyhow::Result<()> {
    Ok(())
}

fn persist_text(_text: String) -> anyhow::Result<()> {
    Ok(())
}

fn handle_other_outcome(_outcome: TurnOutcome) -> anyhow::Result<()> {
    Ok(())
}

fn terminal_value_match(result: TurnReport) -> anyhow::Result<()> {
    // docs:start:terminal-value-match
    match result.outcome {
        TurnOutcome::Finished(TurnFinish::FinalValue { value }) => {
            // Same value already arrived as TurnEvent::FinalValue.
            persist_typed_value(value)?;
        }
        TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) => persist_text(text)?,
        other => handle_other_outcome(other)?,
    }
    // docs:end:terminal-value-match
    Ok(())
}

async fn finish_schema(core: &lash::LashCore) -> anyhow::Result<()> {
    // docs:start:finish-schema
    use lash::rlm::{
        RLM_PROTOCOL_PLUGIN_ID, RlmCreateExtras, RlmDialect, RlmFinalAnswerFormat,
        RlmTurnBuilderExt as _,
    };

    // Durable session facts are stated once through the plugin options seam and
    // applied as a guarded set-if-unset write (ADR 0066).
    let session = core
        .session("analysis")
        .plugin_option(
            RLM_PROTOCOL_PLUGIN_ID,
            RlmCreateExtras {
                dialect: Some(RlmDialect::Typescript),
                final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                ..RlmCreateExtras::default()
            },
        )?
        .open()
        .await?;

    let result = session
        .turn(TurnInput::text("Return a risk rating."))
        .require_finish_schema(serde_json::json!({
            "type": "object",
            "required": ["rating"],
            "properties": {
                "rating": { "type": "string" }
            },
            "additionalProperties": false
        }))?
        .run()
        .await?;
    // docs:end:finish-schema
    Ok(())
}

#[cfg(test)]
mod asserted_examples {
    use std::convert::Infallible;
    use std::future;
    use std::time::Duration;

    #[tokio::test]
    async fn cancellation_tree_stops_borrowed_owned_and_guarded_work() {
        let parent: lash::CancellationToken = lash::CancellationToken::new();
        let child = lash::CancellationToken::child_token(&parent);
        let borrowed_work = lash::CancellationToken::run_until_cancelled(&child, future::pending());
        lash::CancellationToken::cancel(&parent);

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), borrowed_work).await,
            Ok(None::<Infallible>),
            "cancelling a parent must stop work guarded by its child"
        );
        assert!(lash::CancellationToken::is_cancelled(&child));
        tokio::time::timeout(
            Duration::from_secs(1),
            lash::CancellationToken::cancelled(&child),
        )
        .await
        .expect("borrowed cancellation waiter must complete");

        let owned = lash::CancellationToken::new();
        let owned_waiter = tokio::spawn(lash::CancellationToken::cancelled_owned(owned.clone()));
        let owned_work = tokio::spawn(lash::CancellationToken::run_until_cancelled_owned(
            owned.clone(),
            future::pending::<Infallible>(),
        ));
        lash::CancellationToken::cancel(&owned);
        tokio::time::timeout(Duration::from_secs(1), owned_waiter)
            .await
            .expect("owned cancellation waiter must complete")
            .expect("owned cancellation waiter must not panic");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), owned_work)
                .await
                .expect("owned guarded work must complete")
                .expect("owned guarded work must not panic"),
            None,
            "owned guarded work must stop on cancellation"
        );

        let guarded = lash::CancellationToken::new();
        let observed_guarded = guarded.clone();
        drop(lash::CancellationToken::drop_guard(guarded));
        assert!(
            lash::CancellationToken::is_cancelled(&observed_guarded),
            "dropping an owned guard must publish cancellation"
        );

        let borrowed_guarded = lash::CancellationToken::new();
        drop(lash::CancellationToken::drop_guard_ref(&borrowed_guarded));
        assert!(
            lash::CancellationToken::is_cancelled(&borrowed_guarded),
            "dropping a borrowed guard must publish cancellation"
        );
    }

    #[test]
    fn cancellation_evidence_is_read_off_the_turn_outcome() {
        let evidence = lash::TurnCancellationEvidence::internal("incident-summary-42");
        assert_eq!(evidence.request_id, "internal:incident-summary-42");

        let cancelled = lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled {
            evidence: evidence.clone(),
        });
        // A cancelled outcome names the request that stopped it.
        assert_eq!(lash::TurnOutcome::cancellation(&cancelled), Some(&evidence));

        let finished = lash::TurnOutcome::Finished(lash::TurnFinish::AssistantMessage {
            text: "summary".to_string(),
        });
        // Evidence exists only on a cancelled outcome.
        assert_eq!(lash::TurnOutcome::cancellation(&finished), None);
    }
}
