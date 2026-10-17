//! L10 and L22(a)'s enricher and span-budget clauses and L22(c)
//! (FIG-6300): one enricher sees every exported span, producers before
//! their sampling decision, with the same stable ids on every owner; it
//! contributes through one bounded writer against the span's budget after
//! the registry's reserve, and every lash attribute survives.

use super::tracing::{text_call, tool_call};
use super::*;

use lash::TurnInput;
use lash::tracing::otel::trace::{Link, SpanKind, TraceId, TraceState};
use lash::tracing::otel::{Context, KeyValue};
use lash::tracing::{
    RefinableKey, SpanEnricher, SpanEnrichment, SpanView, TraceOmission, TraceOmissionSiteKind,
    TraceSpanKind, WriterRemaining, attribute_reserve,
};
use lash_core::testing::runtime_helpers::{EchoTool, mock_provider};
use lash_trace::otel::{OtelOptions, OtelTelemetry};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, Sampler, SamplingDecision, SamplingResult, SdkTracerProvider,
    ShouldSample, SpanData, SpanLimits,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// An owner's exporter, and `builder` exporting to it under `sampler`,
/// with `options`; the SDK keeps `sdk_attributes` attributes a span.
pub(super) fn exporting(
    builder: lash::LashCoreBuilder,
    sampler: impl ShouldSample + 'static,
    options: OtelOptions,
    sdk_attributes: Option<u32>,
) -> (
    (InMemorySpanExporter, lash::tracing::LashTracerProvider),
    lash::LashCoreBuilder,
) {
    let spans = InMemorySpanExporter::default();
    let mut sdk = SdkTracerProvider::builder().with_simple_exporter(spans.clone());
    if let Some(limit) = sdk_attributes {
        sdk = sdk.with_span_limits(SpanLimits {
            max_attributes_per_span: limit,
            ..SpanLimits::default()
        });
    }
    let tracer = lash::tracing::LashTracerProvider::new(
        sdk,
        sampler,
        lash_trace::otel::sdk::trace::RandomIdGenerator::default(),
    );
    let meter = SdkMeterProvider::builder().build();
    let builder = builder.telemetry(OtelTelemetry::new(&tracer, &meter, options));
    ((spans, tracer), builder)
}

pub(super) fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| attribute.value.to_string())
}

fn count(span: &SpanData, key: &str) -> usize {
    span.attributes
        .iter()
        .filter(|attribute| attribute.key.as_str() == key)
        .count()
}

/// Until `ready` holds, within [`WITHIN`].
async fn until(what: &str, ready: impl Fn() -> bool + Send + Sync) {
    tokio::time::timeout(WITHIN, async {
        while !ready() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

const USER_SESSION: &str = "l10-user";
const BLOCKED_SESSION: &str = "l10-blocked";
/// The host's user for [`USER_SESSION`]: a token no lash row may hold.
const U1: &str = "u1-host-user-7c1f";
const RENAMED: &str = "chat renamed-by-host";

/// One enricher call: the owner, the span and the ids it was shown, and
/// its place in the order of enricher and sampler calls.
#[derive(Clone, Debug)]
struct Seen {
    owner: &'static str,
    kind: TraceSpanKind,
    name: String,
    span: lash_trace::W3cSpanId,
    ids: lash_trace::TraceIds,
    at: usize,
    provider_set: std::result::Result<(), TraceOmission>,
    reserved_set: std::result::Result<(), TraceOmission>,
}

#[derive(Default)]
struct Journal {
    clock: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
    /// Each sampler call: its place in the order, the span's name and the
    /// `user` it saw.
    sampled: Mutex<Vec<(usize, String, Option<String>)>>,
}

/// The host's enricher: maps the session to its user from a host table,
/// names and prices model calls and replaces their provider.
struct HostEnricher {
    owner: &'static str,
    journal: Arc<Journal>,
}

impl SpanEnricher for HostEnricher {
    fn enrich(&self, view: &SpanView<'_>, out: &mut SpanEnrichment) {
        let user = match view
            .ids
            .session
            .as_ref()
            .map(ToString::to_string)
            .as_deref()
        {
            Some(USER_SESSION) => Some(U1),
            Some(BLOCKED_SESSION) => Some("blocked"),
            _ => None,
        };
        if let Some(user) = user {
            out.set("user", user).expect("a host key is admitted");
        }
        if view.kind == TraceSpanKind::Model {
            out.rename(RENAMED).expect("a short name is admitted");
            out.set("gen_ai.usage.cost", 0.25)
                .expect("a host key is not checked against the registry");
            out.replace(RefinableKey::GenAiProviderName, "openrouter")
                .expect("a replacement is admitted");
        }
        let provider_set = out.set("gen_ai.provider.name", "anthropic");
        let reserved_set = out.set("lash.turn.id", "forged");
        self.journal.seen.lock().unwrap().push(Seen {
            owner: self.owner,
            kind: view.kind,
            name: view.default_name.to_owned(),
            span: view.position.span.span_id(),
            ids: view.ids.clone(),
            at: self.journal.clock.fetch_add(1, Ordering::SeqCst),
            provider_set,
            reserved_set,
        });
    }
}

/// The host's sampler: drops the producers whose enriched `user` is
/// `blocked`.
#[derive(Clone)]
struct HostSampler(Arc<Journal>);

impl std::fmt::Debug for HostSampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostSampler")
    }
}

impl ShouldSample for HostSampler {
    fn should_sample(
        &self,
        _: Option<&Context>,
        _: TraceId,
        name: &str,
        _: &SpanKind,
        attributes: &[KeyValue],
        _: &[Link],
    ) -> SamplingResult {
        let user = attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == "user")
            .map(|attribute| attribute.value.to_string());
        let blocked = user.as_deref() == Some("blocked");
        self.0.sampled.lock().unwrap().push((
            self.0.clock.fetch_add(1, Ordering::SeqCst),
            name.to_owned(),
            user,
        ));
        SamplingResult {
            decision: if blocked {
                SamplingDecision::Drop
            } else {
                SamplingDecision::RecordAndSample
            },
            attributes: Vec::new(),
            trace_state: TraceState::default(),
        }
    }
}

/// Fails if any file under `directory` holds `token`.
#[allow(
    clippy::disallowed_methods,
    reason = "the law scans the store files the test's own stores wrote"
)]
pub(super) fn assert_not_stored(directory: &std::path::Path, token: &str) {
    for entry in std::fs::read_dir(directory).expect("a store directory") {
        let path = entry.expect("a store file").path();
        if path.is_dir() {
            assert_not_stored(&path, token);
            continue;
        }
        let bytes = std::fs::read(&path).expect("read a store file");
        assert!(
            !bytes
                .windows(token.len())
                .any(|window| window == token.as_bytes()),
            "{} holds {token}",
            path.display()
        );
    }
}

pub(super) async fn file_backend(path: &std::path::Path) -> lash_core::Backend {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(path, lash_sqlite_store::SqliteSynchronous::Normal)
            .await
            .expect("open the SQLite file store"),
    );
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    lash_conformance::backend_over(stores)
}

fn enriched(owner: &'static str, journal: &Arc<Journal>) -> OtelOptions {
    OtelOptions {
        enricher: Some(Arc::new(HostEnricher {
            owner,
            journal: journal.clone(),
        })),
        ..OtelOptions::standard()
    }
}

/// L10: a send under a host-chosen turn id is admitted by an owner that
/// serves nothing and runs on another. The enricher is called once for
/// every exported span, the producer's before the sampler decides; both
/// owners show it the same ids; its user reaches every span and no store
/// row; its rename, cost and provider replacement survive, and its `set`
/// of a registry or `lash.` key is refused and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_enricher_sees_every_span_before_sampling_on_every_owner_and_nothing_is_stored()
-> Result<()> {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let database = directory.path().join("lash.db");
    let journal = Arc::new(Journal::default());
    let sink = Arc::new(lash::testing::RecordingTraceSink::new());
    let ((first_spans, _first), first) = exporting(
        explicit_ephemeral_facets(LashCore::standard_builder(file_backend(&database).await))
            .serve_test_llm_profile(
                mock_provider(Vec::new()).into_handle(),
                mock_llm_profile_spec(),
            )
            .serve_sessions(false)
            .trace_sink(sink.clone()),
        HostSampler(journal.clone()),
        enriched("first", &journal),
        None,
    );
    let first = first.build(lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new("l10-admitting"),
        lash_core::LeaseIncarnationId::new("l10-admitting-boot"),
    ))?;
    let mut sends = Vec::new();
    for (session, turn) in [
        (USER_SESSION, "l10-turn"),
        (BLOCKED_SESSION, "l10-blocked-turn"),
    ] {
        let receipt = first
            .session(lash::SessionId::parse(session).expect("an id"))
            .create(lash::SessionCreation::root(mock_session_spec()))
            .await?
            .send(TurnInput::text("enriched"))
            .id(lash::TurnId::parse(turn).expect("an id"))
            .await?;
        sends.push(receipt);
    }
    let ((second_spans, _second), second) = exporting(
        explicit_ephemeral_facets(LashCore::standard_builder(file_backend(&database).await))
            .serve_test_llm_profile(
                mock_provider(vec![text_call("done"), text_call("done")]).into_handle(),
                mock_llm_profile_spec(),
            )
            .trace_sink(sink.clone()),
        Sampler::AlwaysOn,
        enriched("second", &journal),
        None,
    );
    let second = second.build(lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new("l10-running"),
        lash_core::LeaseIncarnationId::new("l10-running-boot"),
    ))?;
    until("the second owner ends both turns", || {
        sink.records()
            .iter()
            .filter(|record| record.event.kind().as_str() == "turn_completed")
            .count()
            == 2
    })
    .await;
    second.flush_trace_sink()?;
    first.shutdown().await?;
    second.shutdown().await?;

    let first_exported = first_spans.get_finished_spans().expect("exported spans");
    let second_exported = second_spans.get_finished_spans().expect("exported spans");
    let seen = journal.seen.lock().unwrap().clone();
    let sampled = journal.sampled.lock().unwrap().clone();
    for (owner, exported) in [("first", &first_exported), ("second", &second_exported)] {
        for span in exported.iter() {
            let calls: Vec<_> = seen
                .iter()
                .filter(|seen| {
                    seen.owner == owner
                        && seen.span.to_bytes() == span.span_context.span_id().to_bytes()
                })
                .collect();
            assert_eq!(calls.len(), 1, "{} is enriched once on {owner}", span.name);
            assert_eq!(
                attribute(span, "user").as_deref(),
                Some(U1),
                "{} carries the host's user",
                span.name
            );
            assert_ne!(
                attribute(span, "lash.turn.id").as_deref(),
                Some("forged"),
                "a lash key keeps lash's value"
            );
            assert!(
                attribute(span, "gen_ai.provider.name").as_deref() != Some("anthropic"),
                "a refinable key is changed only by replace"
            );
        }
    }
    assert!(
        first_exported.iter().any(|span| span.name == "lash.send"),
        "the admitting owner exports the send: {first_exported:#?}"
    );
    assert!(
        second_exported
            .iter()
            .any(|span| span.name.starts_with("invoke_agent")),
        "the running owner exports the turn"
    );
    for refused in &seen {
        assert_eq!(refused.provider_set, Err(TraceOmission::UseReplace));
        assert_eq!(refused.reserved_set, Err(TraceOmission::ReservedKey));
    }

    // The producer is enriched before the sampler decides, and the sampler
    // sees what the enricher derived.
    let producers: Vec<_> = seen
        .iter()
        .filter(|seen| seen.kind == TraceSpanKind::Producer && seen.name == "lash.send")
        .collect();
    let decisions: Vec<_> = sampled
        .iter()
        .filter(|(_, name, _)| name == "lash.send")
        .collect();
    assert_eq!(producers.len(), 2, "each send's producer is enriched once");
    assert_eq!(decisions.len(), 2, "the sampler decides each send once");
    for (producer, (decided_at, _, user)) in producers.iter().zip(decisions) {
        assert!(producer.at < *decided_at, "enriched before the decision");
        let expected = match producer
            .ids
            .session
            .as_ref()
            .map(ToString::to_string)
            .as_deref()
        {
            Some(USER_SESSION) => U1,
            _ => "blocked",
        };
        assert_eq!(user.as_deref(), Some(expected), "the sampler sees the user");
    }
    assert!(
        first_exported
            .iter()
            .chain(&second_exported)
            .all(|span| attribute(span, "user").as_deref() != Some("blocked")),
        "the dropped trace exports nothing"
    );

    // The same session, turn and send ids on both owners.
    let ids_on = |owner: &str| -> Vec<lash_trace::TraceIds> {
        seen.iter()
            .filter(|seen| {
                seen.owner == owner
                    && seen
                        .ids
                        .session
                        .as_ref()
                        .map(ToString::to_string)
                        .as_deref()
                        == Some(USER_SESSION)
            })
            .map(|seen| seen.ids.clone())
            .collect()
    };
    // The producer is enriched before admission mints the send's id, so
    // the admitting owner sees the session and the host's turn; the running
    // owner sees the send too. Every id either owner sees is the same.
    let send = sends[0].input_id().to_string();
    for (owner, sees_send) in [("first", false), ("second", true)] {
        let ids = ids_on(owner);
        assert!(!ids.is_empty(), "{owner} enriched the user's spans");
        assert!(
            ids.iter().any(|ids| {
                ids.turn.as_ref().map(ToString::to_string).as_deref() == Some("l10-turn")
                    && (!sees_send
                        || ids.send.as_ref().map(ToString::to_string).as_deref()
                            == Some(send.as_str()))
            }),
            "{owner} sees the host's turn id and the send's id: {ids:#?}"
        );
        for ids in &ids {
            assert!(
                ids.turn
                    .as_ref()
                    .is_none_or(|turn| turn.as_str() == "l10-turn")
            );
            assert!(ids.send.as_ref().is_none_or(|input| input.as_str() == send));
        }
    }

    // The model call is renamed, priced and attributed to the replaced
    // provider, once.
    let chats: Vec<_> = second_exported
        .iter()
        .filter(|span| span.name == RENAMED)
        .collect();
    assert_eq!(
        chats.len(),
        1,
        "the user's model call: {second_exported:#?}"
    );
    assert_eq!(
        attribute(chats[0], "gen_ai.usage.cost").as_deref(),
        Some("0.25")
    );
    assert_eq!(
        attribute(chats[0], "gen_ai.provider.name").as_deref(),
        Some("openrouter")
    );
    assert_eq!(count(chats[0], "gen_ai.provider.name"), 1);

    // Nothing the host supplied is stored.
    assert_not_stored(directory.path(), U1);
    Ok(())
}

/// What the tool span's enricher saw: its budget at entry and each `set`'s
/// answer.
#[derive(Default)]
struct Budgeted {
    entry: Mutex<Option<WriterRemaining>>,
    answers: Mutex<Vec<std::result::Result<(), TraceOmission>>>,
}

const HOST_KEYS: usize = 200;
/// 200 values totalling 300 KiB.
const HOST_VALUE_BYTES: usize = 300 * 1024 / HOST_KEYS;

/// An enricher that sets [`HOST_KEYS`] keys totalling 300 KiB on the tool
/// span.
struct Flooding(Arc<Budgeted>);

impl SpanEnricher for Flooding {
    fn enrich(&self, view: &SpanView<'_>, out: &mut SpanEnrichment) {
        if view.kind != TraceSpanKind::Tool {
            return;
        }
        *self.0.entry.lock().unwrap() = Some(out.remaining());
        let value = "x".repeat(HOST_VALUE_BYTES);
        let answers = (0..HOST_KEYS)
            .map(|index| out.set(format!("host.k{index:03}"), value.clone()))
            .collect();
        *self.0.answers.lock().unwrap() = answers;
    }
}

/// One served turn that calls `echo_tool`, under the flooding enricher and
/// an SDK keeping `sdk_attributes` a span: the exported tool span, what
/// the enricher saw, and the core's omission report.
async fn flooded_tool_span(
    sdk_attributes: Option<u32>,
) -> Result<(SpanData, Arc<Budgeted>, lash::tracing::TraceOmissionReport)> {
    let budgeted = Arc::new(Budgeted::default());
    let ((spans, _tracer), builder) = exporting(
        explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(
            mock_provider(vec![
                tool_call("call-echo", "echo_tool", r#"{"value":"flooded"}"#),
                text_call("done"),
            ])
            .into_handle(),
            mock_llm_profile_spec(),
        )
        .tools(lash::tool_provider_key!("tools"), Arc::new(EchoTool)),
        Sampler::AlwaysOn,
        OtelOptions {
            enricher: Some(Arc::new(Flooding(budgeted.clone()))),
            ..OtelOptions::standard()
        },
        sdk_attributes,
    );
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let output = created_session(&core, lash::SessionId::parse("l22-flooded").expect("an id"))
        .await
        .open()
        .await?
        .send(TurnInput::text("call the tool"))
        .output()
        .await?;
    assert!(
        matches!(output.outcome, lash::TurnOutcome::Finished(_)),
        "{:?}",
        output.outcome
    );
    until("the tool span is exported", || {
        spans
            .get_finished_spans()
            .expect("exported spans")
            .iter()
            .any(|span| span.name.starts_with("execute_tool"))
    })
    .await;
    let span = spans
        .get_finished_spans()
        .expect("exported spans")
        .into_iter()
        .find(|span| span.name.starts_with("execute_tool"))
        .expect("the tool span");
    let report = core.trace_omissions();
    core.shutdown().await?;
    Ok((span, budgeted, report))
}

/// L22(a)'s enricher and span-budget clauses: the enricher's budget at
/// entry is the tool span's contribution budget after the registry's
/// reserve (no body annotation or hook reference precedes it here); keys
/// past it are refused `Attributes`, the assembled span stays within
/// `attributes_per_span` and `span_contribution_bytes`, every lash
/// attribute is present, and the core counts each refusal at the span.
/// L22(c): an SDK keeping only `attribute_reserve(Tool)` attributes drops
/// contributions and never a lash attribute.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_enricher_is_bounded_by_the_spans_budget_after_the_reserve_and_lash_attributes_survive()
-> Result<()> {
    let limits = lash::tracing::TraceLimits::standard();
    let reserve = attribute_reserve(TraceSpanKind::Tool);
    let (span, budgeted, report) = flooded_tool_span(None).await?;
    let entry = budgeted
        .entry
        .lock()
        .unwrap()
        .expect("the tool span is enriched");
    assert_eq!(
        entry,
        WriterRemaining {
            attributes: limits.attributes_per_span() - reserve,
            bytes: limits.span_contribution_bytes(),
        }
    );
    let answers = budgeted.answers.lock().unwrap().clone();
    let admitted = answers.iter().filter(|answer| answer.is_ok()).count();
    assert_eq!(
        u32::try_from(admitted).expect("a count"),
        entry.attributes,
        "the count budget runs out first"
    );
    assert!(
        answers[admitted..]
            .iter()
            .all(|answer| *answer == Err(TraceOmission::Attributes)),
        "every key past the budget is refused Attributes"
    );
    let refused = HOST_KEYS - admitted;

    let host: Vec<_> = span
        .attributes
        .iter()
        .filter(|attribute| attribute.key.as_str().starts_with("host."))
        .collect();
    let lash: Vec<_> = span
        .attributes
        .iter()
        .filter(|attribute| !attribute.key.as_str().starts_with("host."))
        .map(|attribute| attribute.key.as_str().to_owned())
        .collect();
    assert_eq!(host.len(), admitted);
    assert!(span.attributes.len() <= limits.attributes_per_span() as usize);
    assert!(
        lash.len() <= reserve as usize,
        "lash stays within its reserve: {lash:?}"
    );
    let contributed: usize = host
        .iter()
        .map(|attribute| attribute.key.as_str().len() + attribute.value.as_str().len())
        .sum();
    assert!(contributed as u64 <= limits.span_contribution_bytes());
    for key in [
        "gen_ai.tool.name",
        "gen_ai.tool.call.id",
        "lash.record.id",
        "lash.session.id",
        "lash.trace.omitted",
        "lash.trace.omitted.count",
    ] {
        assert!(
            lash.iter().any(|kept| kept == key),
            "{key} is present: {lash:?}"
        );
    }
    assert_eq!(
        attribute(&span, "lash.trace.omitted.count"),
        Some(refused.to_string())
    );
    assert_eq!(
        report.get(TraceOmissionSiteKind::Span, TraceOmission::Attributes),
        u64::try_from(refused).expect("a count")
    );
    assert_eq!(report.total(), u64::try_from(refused).expect("a count"));

    let (limited, _, _) = flooded_tool_span(Some(reserve)).await?;
    let mut kept: Vec<_> = limited
        .attributes
        .iter()
        .filter(|attribute| !attribute.key.as_str().starts_with("host."))
        .map(|attribute| attribute.key.as_str().to_owned())
        .collect();
    let mut expected = lash.clone();
    kept.sort();
    expected.sort();
    assert_eq!(kept, expected, "every lash attribute survives the SDK");
    let kept_host = limited
        .attributes
        .iter()
        .filter(|attribute| attribute.key.as_str().starts_with("host."))
        .count();
    assert!(limited.dropped_attributes_count > 0);
    assert_eq!(
        kept_host + limited.dropped_attributes_count as usize,
        admitted,
        "the SDK drops only contributions"
    );
    Ok(())
}
