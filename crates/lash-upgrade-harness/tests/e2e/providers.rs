//! H1 provider and external-oracle witnesses. These cheap witnesses use a
//! real production HTTP client and Lash core over SQLite and the Restate
//! server double; live crash scenarios are driven by the common controller.

use std::time::Duration;

use anyhow::{Result, ensure};
use lash::provider::{ProviderFailureKind, ProviderReliability};
use lash_upgrade_harness::e2e::provider_http::ledger::{EffectDelivery, EffectLedger};
use lash_upgrade_harness::e2e::provider_http::transcript::{HttpTranscript, StreamEnd};
use lash_upgrade_harness::e2e::provider_http::{RecordedHttpFixture, TransportEvent};
use lash_upgrade_harness::node::e2e_provider;

const WAIT: Duration = Duration::from_secs(120);
const RATE_LIMIT: &[u8] = include_bytes!("../../testdata/e2e/providers/s26-rate-limit.json");
const DISCONNECT: &[u8] =
    include_bytes!("../../testdata/e2e/providers/s26-partial-disconnect.json");
const AUTH: &[u8] = include_bytes!("../../testdata/e2e/providers/s27-auth-next-run.json");

async fn start(bytes: &[u8], dir: &std::path::Path) -> Result<RecordedHttpFixture> {
    RecordedHttpFixture::start(
        ([127, 0, 0, 1], 0).into(),
        HttpTranscript::from_json(bytes)?,
        &dir.join("effects.jsonl"),
    )
    .await
}

async fn core(
    fixture: &RecordedHttpFixture,
) -> Result<(lash::LashCore, lash_restate_test::RestateTestBackend)> {
    let double =
        lash_restate_test::backend(0x4932, lash_restate_test::ServerConfig::default()).await?;
    let core = e2e_provider::core(
        double.lash_backend(),
        &fixture.base_url(),
        ProviderReliability::default()
            .max_attempts(2)
            .base_delay_ms(0)
            .max_delay_ms(0),
    )?;
    Ok((core, double))
}

fn record(name: &str, receipt: &impl serde::Serialize) -> Result<()> {
    if let Some(dir) = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR") {
        let path = std::path::PathBuf::from(dir).join(format!("{name}.json"));
        std::fs::write(path, serde_json::to_vec_pretty(receipt)?)?;
    }
    Ok(())
}

/// R6/L14/L17: one logical model call retries an observed HTTP 429, an
/// observer may disconnect, and late reattachment sees one committed answer
/// with the same usage and request count as the wire transcript.
#[tokio::test]
async fn s26_rate_limit_and_observer_reconnect_commit_one_answer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = start(RATE_LIMIT, dir.path()).await?;
    let (core, double) = core(&fixture).await?;
    let proof: Result<()> = tokio::time::timeout(WAIT, async {
        let session = e2e_provider::session(&core, "s26-rate-limit").await?;
        let accepted = session.send(lash::TurnInput::text("answer once")).id("s26-run").await?;
        let input = accepted.input_id().clone();
        let observer = accepted.events();
        // Keep an outcome follower while dropping the independent observer.
        // A late durable report intentionally omits per-turn call ledgers.
        let follower = tokio::spawn(async move { accepted.output().await });
        fixture.wait_for(WAIT, |event| matches!(event,
            TransportEvent::BarrierEntered { barrier, .. } if barrier == "answer-started"
        )).await?;
        drop(observer);
        fixture.release("answer-started")?;
        let output = follower.await??;
        record("s26-rate-limit-output", &output)?;
        ensure!(output.assistant_message() == Some("one answer"), "wrong committed assistant answer");
        ensure!(output.result.acceptance.as_ref().is_some_and(|receipt| receipt.input_id == input), "reattachment changed ingress identity");
        let completions = double.server().invocations().into_iter().flat_map(|invocation| {
            double.server().journal(&invocation.id).unwrap_or_default().into_iter()
                .filter_map(|entry| entry.run_completion().and_then(Result::ok))
                // Atomic effect-group children journal their Completed
                // envelope, rather than the bare inner runtime outcome.
                .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .filter(|value| value["type"] == "completed")
                .filter_map(|value| value["outcome"].get("Ok").cloned())
                .filter_map(|value| serde_json::from_value::<lash_core::RuntimeEffectOutcome>(value).ok())
                .filter_map(|outcome| match outcome {
                    lash_core::RuntimeEffectOutcome::LlmCall { result, call_record, .. } => Some((result, call_record)),
                    _ => None,
                })
        }).collect::<Vec<_>>();
        ensure!(completions.len() == 1, "journal contains {} logical model calls", completions.len());
        let (result, call_record) = &completions[0];
        let response = result.as_ref().as_ref().map_err(|error| anyhow::anyhow!("journaled provider failure: {error:?}"))?;
        let call = call_record.as_ref().ok_or_else(|| anyhow::anyhow!("journaled completion has no attempt ledger"))?;
        let attempts = &call.attempts;
        ensure!(attempts.len() == 2 && attempts[0].ordinal == 1 && attempts[1].ordinal == 2, "reported 429 retry ordinals differ");
        ensure!(attempts[0].error.as_ref().is_some_and(|error| error.class == ProviderFailureKind::Http), "429 lost typed HTTP failure");
        ensure!(response.usage.input_tokens == 11 && response.usage.output_tokens == 2, "journal usage disagrees with transcript");
        if output.result.source == lash::turn::ReportSource::Live {
            ensure!(output.result.llm_calls == vec![call.clone()], "live receipts disagree with journal");
            ensure!(output.result.usage.input_tokens == 11 && output.result.usage.output_tokens == 2, "live usage disagrees with journal");
        }
        record("s26-rate-limit-journal-provider", &serde_json::json!({ "call": call, "response": response }))?;
        let view = session.read_view();
        ensure!(view.token_usage().input_tokens == 11 && view.token_usage().output_tokens == 2, "committed usage disagrees with journal");
        let assistants = view.messages().iter().filter(|message| message.role == lash::messages::MessageRole::Assistant).count();
        ensure!(assistants == 1, "observer reconnect duplicated the assistant message");
        let late = session.attach(input).output().await?;
        ensure!(late.assistant_message() == output.assistant_message(), "late follower disagrees with committed output");
        record("s26-rate-limit-output", &output)?;
        Ok(())
    }).await.unwrap_or_else(|error| Err(error.into()));
    let receipt = fixture.finish().await?;
    record("s26-rate-limit-http", &receipt)?;
    receipt.verify()?;
    proof
}

/// R6/L14/L17: real HTTP framing is interrupted after visible Chat output.
/// The non-retryable transport policy fails typed without a second generation or a
/// speculative committed answer. This is S26's separate reset variant.
#[tokio::test]
async fn s26_partial_stream_disconnect_refuses_unsafe_regeneration() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = start(DISCONNECT, dir.path()).await?;
    let (core, _double) = core(&fixture).await?;
    let proof: Result<()> = tokio::time::timeout(WAIT, async {
        let session = e2e_provider::session(&core, "s26-disconnect").await?;
        let output = session
            .send(lash::TurnInput::text("partial answer"))
            .output()
            .await?;
        record("s26-disconnect-output", &output)?;
        ensure!(
            matches!(
                output.result.outcome,
                lash::TurnOutcome::Stopped(lash::TurnStop::ProviderError)
            ),
            "partial stream did not fail the Run"
        );
        ensure!(
            output.result.outcome.cancellation().is_none(),
            "provider read failure became cancellation"
        );
        ensure!(
            output
                .result
                .errors
                .iter()
                .any(
                    |issue| issue.provider_failure_kind == Some(ProviderFailureKind::Transport)
                        && issue.retryable == Some(false)
                ),
            "partial stream lost its typed non-retryable transport policy: {:?}",
            output.result.errors
        );
        ensure!(
            output.result.llm_calls.len() == 1 && output.result.llm_calls[0].attempts.len() == 1,
            "partial output caused an unsafe second generation"
        );
        ensure!(
            !session
                .read_view()
                .messages()
                .iter()
                .any(|message| message.role == lash::messages::MessageRole::Assistant),
            "partial answer committed speculatively"
        );
        record("s26-disconnect-output", &output)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|error| Err(error.into()));
    let receipt = fixture.finish().await?;
    record("s26-disconnect-http", &receipt)?;
    receipt.verify()?;
    proof
}

/// R6/R7: authentication failure terminates Failed (never Cancelled), and
/// a fresh Run in the same session answers without a stuck owner or retry.
#[tokio::test]
async fn s27_authentication_failure_permits_the_next_run() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = start(AUTH, dir.path()).await?;
    let (core, _double) = core(&fixture).await?;
    let proof: Result<()> = tokio::time::timeout(WAIT, async {
        let session = e2e_provider::session(&core, "s27-auth").await?;
        let first = session
            .send(lash::TurnInput::text("invalid credentials"))
            .id("auth-run-0")
            .output()
            .await?;
        ensure!(
            matches!(
                first.result.outcome,
                lash::TurnOutcome::Stopped(lash::TurnStop::ProviderError)
            ),
            "401 did not terminate as provider failure"
        );
        ensure!(
            first.result.outcome.cancellation().is_none(),
            "401 became cancellation"
        );
        ensure!(
            first.result.errors.iter().any(|issue| issue.kind
                == lash::turn::TurnFailureKind::LlmProvider
                && issue.provider_failure_kind == Some(ProviderFailureKind::Auth)),
            "401 lost provider authentication classification"
        );
        ensure!(
            first.result.llm_calls.len() == 1 && first.result.llm_calls[0].attempts.len() == 1,
            "authentication failure was retried"
        );
        let second = session
            .send(lash::TurnInput::text("fresh valid request"))
            .id("auth-run-1")
            .output()
            .await?;
        ensure!(
            second.assistant_message() == Some("recovered"),
            "next Run stayed stuck after 401"
        );
        ensure!(
            first
                .result
                .acceptance
                .as_ref()
                .map(|receipt| &receipt.input_id)
                != second
                    .result
                    .acceptance
                    .as_ref()
                    .map(|receipt| &receipt.input_id),
            "fresh Run inherited failed ingress identity"
        );
        record("s27-first-output", &first)?;
        record("s27-next-output", &second)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|error| Err(error.into()));
    let receipt = fixture.finish().await?;
    record("s27-http", &receipt)?;
    receipt.verify()?;
    proof
}

/// L02/L22: the outside service dedups ambiguous redelivery and reported
/// retries by call ID across a cold ledger reopen; changed ownership/content
/// and corrupt evidence can never create a new mutation under that ID.
#[tokio::test]
async fn outside_effect_identity_dedups_across_http_redelivery_and_cold_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = start(DISCONNECT, dir.path()).await?;
    let delivery = EffectDelivery {
        owner: "turn-owner".into(),
        run: "logical-run".into(),
        call_id: "call-1".into(),
        attempt: 1,
        payload: serde_json::json!({ "write": "value" }),
    };
    let client = reqwest::Client::new();
    fixture.hold_effect("call-1", 1, "effect-accepted", StreamEnd::Disconnect)?;
    let lost_client = client.clone();
    let lost_url = fixture.effect_url();
    let lost_delivery = delivery.clone();
    let lost_reply =
        tokio::spawn(async move { lost_client.post(lost_url).json(&lost_delivery).send().await });
    fixture
        .wait_for(WAIT, |event| {
            matches!(event,
                TransportEvent::BarrierEntered { barrier, .. } if barrier == "effect-accepted"
            )
        })
        .await?;
    ensure!(
        fixture.receipt()?.mutations == 1,
        "acceptance was not durable before the lost reply"
    );
    fixture.release("effect-accepted")?;
    ensure!(
        lost_reply.await?.is_err(),
        "ambiguous acceptance unexpectedly returned a reply"
    );
    client
        .post(fixture.effect_url())
        .json(&delivery)
        .send()
        .await?
        .error_for_status()?;
    // Consume the provider occurrence as well: effect-only traffic never
    // satisfies a missing provider selection.
    client
        .post(format!("{}/chat/completions", fixture.base_url()))
        .json(&HttpTranscript::from_json(DISCONNECT)?.occurrences[0].body)
        .send()
        .await?
        .bytes()
        .await
        .expect_err("the recorded HTTP response disconnects");
    fixture
        .wait_for(WAIT, |event| {
            matches!(event, TransportEvent::ResponseEnded { .. })
        })
        .await?;
    let receipt = fixture.finish().await?;
    receipt.verify()?;
    ensure!(
        receipt.effects.len() == 2 && receipt.mutations == 1,
        "HTTP redelivery mutated twice"
    );
    let mut ledger = EffectLedger::open(&dir.path().join("effects.jsonl"))?;
    let mut retry = delivery.clone();
    retry.attempt = 2;
    let retried = ledger.accept(retry)?;
    ensure!(
        !retried.mutated && ledger.mutation_count() == 1 && ledger.deliveries().len() == 3,
        "cold reported retry lost call dedup"
    );
    let mut changed = delivery;
    changed.payload = serde_json::json!({ "write": "changed" });
    ensure!(
        ledger.accept(changed.clone()).is_err(),
        "changed-content replay accepted"
    );
    changed.payload = serde_json::json!({ "write": "value" });
    changed.run = "another-run".into();
    ensure!(
        ledger.accept(changed).is_err(),
        "another owner reused the call ID"
    );
    drop(ledger);
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("effects.jsonl"))?
        .write_all(b"{")?;
    ensure!(
        EffectLedger::open(&dir.path().join("effects.jsonl")).is_err(),
        "truncated evidence allowed another mutation"
    );
    record("outside-effects-http", &receipt)
}

/// Section 3/R6: an out-of-order or extra production request latches a
/// failure; neither it nor a zero-match run can reconcile to passing proof.
#[tokio::test]
async fn strict_http_selection_refuses_wrong_order_extra_and_zero_requests() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = start(DISCONNECT, dir.path()).await?;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/chat/completions", fixture.base_url()))
        .json(&serde_json::json!({ "out_of_order": true }))
        .send()
        .await?;
    ensure!(response.status() == 409, "unexpected request was served");
    let response = client
        .post(format!("{}/chat/completions", fixture.base_url()))
        .json(&HttpTranscript::from_json(DISCONNECT)?.occurrences[0].body)
        .send()
        .await?;
    ensure!(response.status() == 200, "expected request was not served");
    response.bytes().await.expect_err("recorded disconnect");
    let extra = client
        .post(format!("{}/chat/completions", fixture.base_url()))
        .json(&serde_json::Value::Null)
        .send()
        .await?;
    ensure!(extra.status() == 409, "extra request was served");
    extra.bytes().await?;
    let receipt = fixture.finish().await?;
    ensure!(
        receipt.verify().is_err() && receipt.violations.len() >= 2,
        "unexpected requests were hidden by a later match"
    );
    let fixture = start(DISCONNECT, dir.path()).await?;
    let empty = fixture.finish().await?;
    ensure!(
        empty.verify().is_err() && empty.matched == 0,
        "zero-match fixture passed"
    );
    record("strict-http-rejections", &receipt)
}
