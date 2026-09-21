//! The two-opener differential oracle (FIG-3429, ADR 0099 §3).
//!
//! A durable effect-group child's replay key is its *identity*; the retained
//! request is its *authority*. A successor process may legitimately recover
//! the group — but only the retained request may run, and only through
//! runners the recovering host resolved for that request. This oracle stands
//! two opener profiles up that differ on every inheritable authority axis the
//! rebind checklist rules, opens the group under profile A, crashes the
//! process that claimed it, and has profile B reopen offering the same replay
//! keys under its own requests.
//!
//! Whatever the binding lets through, the retained children must still belong
//! to A on every observable the request carries — session, frame, lineage,
//! admission, environment, binding, routing — and nothing B resolved for its
//! offered request may run at all.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::LeaseTimings;
use lash_core::runtime::effect::{
    ToolChildAdmission, ToolChildCompletionRouting, ToolChildRequest, ToolChildScope,
};
use lash_core::tool_dispatch::{
    REBIND_FIELDS, RebindDisposition, RebindField, ToolAttemptEffectIdentity,
};
use lash_core::{
    CancellationToken, ChildDrainOutcome, EffectAddress, EffectGroupHandle, EffectHost,
    EffectOpener, ExecutionScope, FrameNodeId, GroupExecutors, GroupWakePolicy, LoserPolicy,
    PreparedToolCall, ProcessExecutionEnvRef, ProcessIncarnation, ProcessRef, RuntimeAttribution,
    RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation, ScopedEffectController,
    SessionId, StoreEffectGroupDrain, ToolDefinition, ToolExecutionGrant, ToolId, ToolManifest,
    ToolRetryPolicy, TurnControlBindingId,
};
use lash_sansio::sync::MutexExt;
use lash_sqlite_store::{SqliteEffectHost, SqliteEffectReplayOptions};

const LEASE: Duration = Duration::from_millis(900);
const POLL: Duration = Duration::from_millis(25);
const AWAIT_BUDGET: Duration = Duration::from_secs(60);
const RUN: LoserPolicy = LoserPolicy::RunToCompletion;

// =============================================================================
// Opener profiles
// =============================================================================

/// One opener's authority: every inheritable fact a retained tool-child
/// request carries, generated so two profiles differ on all of them.
///
/// This is the oracle's fixture contract. The checklist in
/// [`REBIND_FIELDS`] rules which context fields are `Rebound` — taken from the
/// recorded request — and each of those fields has a request-carried axis
/// here that the differential asserts belongs to the admitting opener. `Lent`
/// fields are the opener's deployment wiring and are what the resolver stands
/// in for; `Fresh` fields carry no recorded fact at all.
struct OpenerProfile {
    /// The session the child's work is attributed to (`RebindField::SessionId`).
    session: &'static str,
    /// The turn the turn-arm opener names, when it is a turn.
    turn: &'static str,
    /// The agent frame (`RebindField::AgentFrameId`).
    frame: &'static str,
    /// The scope the request claims the child under (`RebindField::EffectController`
    /// — the child's own admitted controller is scoped to it).
    admitted_scope: ExecutionScope,
    /// The admitted manifest's tool (`RebindField::ToolCatalog`).
    tool: &'static str,
    /// Retry policy the admission pins: (attempts, base ms, max ms) — the
    /// "policy billed" axis.
    retry: (u32, u64, u64),
    /// The call this child executes.
    call: &'static str,
    /// The parent invocation — the child's lineage
    /// (`RebindField::ParentInvocation`).
    parent: &'static str,
    /// The durable cancellation authority.
    binding: &'static str,
    /// The captured environment (`RebindField::ExecutionEnvSpec`).
    env: &'static str,
    /// The opener arm itself: a turn, or one process incarnation.
    process: Option<(&'static str, u64)>,
    /// How the child's completion routes (`RebindField::DirectCompletions`).
    routing: ToolChildCompletionRouting,
    /// `Catalog` vs `Granted`: the grant source the admission records.
    granted: bool,
}

fn profile_a() -> OpenerProfile {
    OpenerProfile {
        session: "session-a",
        turn: "turn-a",
        frame: "frame-a",
        admitted_scope: ExecutionScope::turn("session-a", "turn-a"),
        tool: "tool-a",
        retry: (2, 10, 100),
        call: "call-a",
        parent: "parent-a",
        binding: "binding-a",
        env: "env-a",
        process: None,
        routing: ToolChildCompletionRouting::Durable,
        granted: false,
    }
}

/// The second opener differs in every inheritable field — including the opener
/// arm itself: a process incarnation where A was a turn.
fn profile_b() -> OpenerProfile {
    OpenerProfile {
        session: "session-b",
        turn: "turn-b",
        frame: "frame-b",
        admitted_scope: ExecutionScope::turn("session-b", "turn-b"),
        tool: "tool-b",
        retry: (5, 50, 500),
        call: "call-b",
        parent: "parent-b",
        binding: "binding-b",
        env: "env-b",
        process: Some(("process-b", 7)),
        routing: ToolChildCompletionRouting::ProcessLifetime,
        granted: true,
    }
}

fn manifest(profile: &OpenerProfile) -> ToolManifest {
    let mut manifest = ToolDefinition::raw(
        profile.tool,
        profile.tool,
        "the tool this opener admits",
        serde_json::json!({ "type": "object" }),
        serde_json::json!({ "type": "object" }),
    )
    .manifest();
    manifest.retry_policy =
        ToolRetryPolicy::safe(profile.retry.0, profile.retry.1, profile.retry.2);
    manifest
}

/// Builds the retained request one opener's child would carry — the whole of
/// the authority it was admitted under.
fn request(profile: &OpenerProfile) -> ToolChildRequest {
    let call = PreparedToolCall::from_parts(
        profile.call,
        ToolId::from(profile.tool),
        profile.tool,
        serde_json::json!({ "from": profile.session }),
        None,
        serde_json::Value::Null,
    );
    let admission = if profile.granted {
        ToolChildAdmission::Granted {
            grant: Box::new(
                ToolExecutionGrant::from_definition(ToolDefinition::raw(
                    profile.tool,
                    profile.tool,
                    "the tool this opener admits",
                    serde_json::json!({ "type": "object" }),
                    serde_json::json!({ "type": "object" }),
                ))
                .with_source_id(format!("source-{}", profile.tool))
                .with_execution_binding(serde_json::json!({ "opener": profile.session })),
            ),
        }
    } else {
        ToolChildAdmission::Catalog {
            manifest: Box::new(manifest(profile)),
        }
    };
    let scope = ExecutionScope::turn(profile.session, profile.turn);
    let opener = match profile.process {
        Some((name, incarnation)) => EffectOpener::process(ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        )),
        None => EffectOpener::turn(profile.session, profile.turn),
    };
    let request = ToolChildRequest::new(
        call,
        admission,
        ToolAttemptEffectIdentity::Scalar {
            parent: Some(RuntimeInvocation::effect(
                EffectAddress::new(scope.clone(), profile.parent.to_string())
                    .expect("a valid parent address"),
                RuntimeAttribution::none(),
                "parent",
            )),
        },
        ToolChildScope {
            opener,
            admitted_scope: profile.admitted_scope.clone(),
            session_id: SessionId::from(profile.session),
            agent_frame_id: FrameNodeId::new(profile.frame).expect("a valid frame id"),
        },
        ProcessExecutionEnvRef::new(profile.env),
        profile.routing,
    )
    .with_cancellation_authority(
        TurnControlBindingId::new(profile.binding).expect("a valid binding id"),
    );
    match profile.process {
        Some((name, incarnation)) => request.with_enclosing_process(ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        )),
        None => request,
    }
}

/// The request-carried authority axes, as strings a failing assert can name.
///
/// Seven of these are generated from [`REBIND_FIELDS`]: every field the
/// checklist rules `Rebound` maps to exactly one. The rest are request-native
/// facts the checklist does not name — the call itself, the opener arm, the
/// cancellation authority, the enclosing incarnation — which the ticket's
/// "differ in every inheritable field" clause still demands differ.
const REQUEST_AXES: &[&str] = &[
    "admission",
    "admitted_scope",
    "completion_routing",
    "lineage",
    "execution_env",
    "session",
    "agent_frame",
    "call",
    "opener",
    "cancellation_authority",
    "enclosing_process",
];

/// The request-carried axis a `Rebound` checklist field maps to.
///
/// `Lent` and `Fresh` fields map to none: lent fields are the recovering
/// host's wiring (this fixture's resolver), and fresh fields record nothing.
/// A new field ruled `Rebound` without an axis here fails the fixture's
/// completeness assert — the checklist and the differential cannot drift.
fn checklist_axis(field: RebindField) -> Option<&'static str> {
    match field {
        RebindField::ToolCatalog => Some("admission"),
        RebindField::EffectController => Some("admitted_scope"),
        RebindField::DirectCompletions => Some("completion_routing"),
        RebindField::ParentInvocation => Some("lineage"),
        RebindField::ExecutionEnvSpec => Some("execution_env"),
        RebindField::SessionId => Some("session"),
        RebindField::AgentFrameId => Some("agent_frame"),
        _ => None,
    }
}

/// The canonical value of one axis, so the differential can compare the
/// observed request field by field and name the one that leaked.
fn axis_value(request: &ToolChildRequest, axis: &str) -> String {
    let value = match axis {
        "admission" => serde_json::to_value(&request.admission),
        "admitted_scope" => serde_json::to_value(&request.scope.admitted_scope),
        "completion_routing" => serde_json::to_value(&request.completion_routing),
        "lineage" => serde_json::to_value(&request.attempt_identity),
        "execution_env" => serde_json::to_value(request.execution_env.as_str()),
        "session" => serde_json::to_value((&request.scope.session_id, &request.scope.opener)),
        "agent_frame" => serde_json::to_value(&request.scope.agent_frame_id),
        "call" => serde_json::to_value(&request.call),
        "opener" => serde_json::to_value(&request.scope.opener),
        "cancellation_authority" => serde_json::to_value(&request.cancellation_authority),
        "enclosing_process" => serde_json::to_value(&request.enclosing_process),
        _ => panic!("no such authority axis: {axis}"),
    }
    .expect("every request axis serializes");
    value.to_string()
}

// =============================================================================
// The differential
// =============================================================================

/// The fixture half of the oracle: the two profiles must differ on every
/// request-carried axis, and every `Rebound` checklist field must map to one.
///
/// A profile that forgot an axis — or a checklist field that acquired one —
/// is exactly the silent-narrowing failure this oracle exists to catch, so the
/// fixture's own completeness is asserted before the differential is trusted.
#[test]
fn the_profiles_differ_on_every_axis_and_the_checklist_covers_them() {
    let a = request(&profile_a());
    let b = request(&profile_b());
    for axis in REQUEST_AXES {
        assert_ne!(
            axis_value(&a, axis),
            axis_value(&b, axis),
            "opener profiles must differ on axis `{axis}`: a differential that \
             agrees anywhere asserts nothing about that field"
        );
    }

    let mapped: std::collections::BTreeSet<&'static str> = REBIND_FIELDS
        .iter()
        .filter_map(|field| checklist_axis(*field))
        .collect();
    for field in REBIND_FIELDS {
        match field.disposition() {
            RebindDisposition::Rebound => assert!(
                checklist_axis(*field).is_some(),
                "rebound field {field:?} has no request-carried axis"
            ),
            _ => assert!(
                checklist_axis(*field).is_none(),
                "a {field:?} field ruled lent-or-fresh must not gain a request \
                 axis behind the checklist's back"
            ),
        }
    }
    assert!(
        mapped.len() <= REQUEST_AXES.len(),
        "checklist axes are a subset of the request's"
    );
}

/// The differential itself.
///
/// Phase 1 claims the children under profile A and crashes, so the reopen in
/// phase 2 meets lapsed claims it could take over — the exact window a
/// same-key impostor needs. Profile B reopens offering the same replay keys
/// under its own requests, staged with executors that record the request they
/// are handed. Its resolver refuses every retained child, so under the leak
/// the impostor executors are the only runners that could ever run, and under
/// the binding nothing runs at all. Phase 3 recovers honestly and asserts
/// every observable belongs to A.
#[tokio::test]
async fn a_child_runs_under_the_opener_that_admitted_it_not_the_one_reoffering_its_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("two-opener.db");
    let prefix = format!("two-opener-{:x}", fastrand::u64(..));
    let scope = ExecutionScope::runtime_operation(format!("{prefix}-scope"));
    let key = format!("{prefix}:group:0");
    let a = profile_a();
    let b = profile_b();
    let request_a = request(&a);
    let request_b = request(&b);

    // Phase 1: the first opener's children are claimed and in flight when the
    // process dies — claimed, journaled, owned by nobody.
    let entered = Arc::new(AtomicUsize::new(0));
    crash_opening(
        &path,
        &scope,
        group(&scope, &key, &request_a),
        vec![parked(&entered), parked(&entered)],
        &entered,
        2,
    );
    until_leases_lapse(&path, &key).await;

    // Phase 2: the second opener reopens the same group under its own
    // requests — same replay keys, different authority — staged with runners
    // that must never run. Its resolver refuses the retained children, so
    // only a runner bound to the *offered* request could execute them.
    let impostor_runs = Arc::new(AtomicUsize::new(0));
    let impostor_saw: Arc<Mutex<Vec<String>>> = Arc::default();
    let spy_b = Arc::new(SpyExecutors::refusing());
    let world_b = world(&path, &spy_b).await;
    let scoped_b = world_b.scoped(scope.clone()).expect("scope");
    let mut handle = open(
        &scoped_b,
        group(&scope, &key, &request_b),
        &spy_b,
        vec![
            impostor(&impostor_runs, &impostor_saw),
            impostor(&impostor_runs, &impostor_saw),
        ],
    )
    .await;

    assert!(
        tokio::time::timeout(
            Duration::from_millis(2 * LEASE.as_millis() as u64),
            scoped_b
                .controller()
                .await_next_settlement(&mut handle, CancellationToken::new()),
        )
        .await
        .is_err(),
        "no settlement arrives: nothing the second opener resolved may run a \
         retained child"
    );
    assert_eq!(
        impostor_runs.load(Ordering::SeqCst),
        0,
        "a runner bound to the offered request may not run a retained child \
         under a different request, however equal its replay key"
    );
    assert!(
        impostor_saw.lock_recover().is_empty(),
        "the impostor was never handed the retained request: {:?}",
        impostor_saw.lock_recover()
    );
    assert_eq!(
        spy_b.asked(),
        vec![child_key(&key, 0), child_key(&key, 1)],
        "the routing question the reopen asks is about the retained children, \
         once each — matching on the key alone would ask the resolver nothing"
    );

    // Phase 3: an honest host still finds both children and settles them under
    // the retained requests — refusing the impostor strands nothing.
    let captured: Arc<Mutex<Vec<ToolChildRequest>>> = Arc::default();
    let spy_c = Arc::new(SpyExecutors::capturing(&captured));
    let world_c = world(&path, &spy_c).await;
    let report = drain_until_no_live_lease(&world_c.group_drain(), &key).await;
    assert_eq!(
        report.settled(),
        2,
        "refusing the impostor strands nothing: {report:?}"
    );
    assert_eq!(spy_c.asked(), vec![child_key(&key, 0), child_key(&key, 1)]);

    let observed = captured.lock_recover().clone();
    assert_eq!(observed.len(), 2, "each retained child ran exactly once");
    for (index, seen) in observed.iter().enumerate() {
        for axis in REQUEST_AXES {
            assert_eq!(
                axis_value(seen, axis),
                axis_value(&request_a, axis),
                "child {index} ran under the wrong authority on axis `{axis}`"
            );
        }
        assert_ne!(
            serde_json::to_string(seen).expect("the observed request serializes"),
            serde_json::to_string(&request_b).expect("the offered request serializes"),
            "child {index} ran the second opener's request"
        );
    }

    scoped_b
        .controller()
        .close_effect_group(handle, RUN)
        .await
        .expect("the caller closes");
}

/// The oracle is only worth trusting if it goes red against the leak it
/// names. The leak is injected here as a resolver double, not a production
/// edit: `SpyExecutors::reoffering_by_key` answers the driver's
/// re-resolution asks by replay key alone — "reuse the offered runner
/// regardless of the retained envelope," the match the retained-envelope
/// binding exists to stop mattering. It enters through the same
/// `register_group_executors` staging seam the honest resolvers use, so the
/// driver under test is byte-identical to the one the green path exercises.
///
/// Every assertion of the differential inverts under the mutant: the
/// impostor-bound runners run, they are handed the *retained* requests, and
/// a settlement arrives.
#[tokio::test]
async fn the_oracle_goes_red_against_a_key_only_resolver() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("two-opener-mutant.db");
    let prefix = format!("two-opener-mutant-{:x}", fastrand::u64(..));
    let scope = ExecutionScope::runtime_operation(format!("{prefix}-scope"));
    let key = format!("{prefix}:group:0");
    let request_a = request(&profile_a());
    let request_b = request(&profile_b());

    let entered = Arc::new(AtomicUsize::new(0));
    crash_opening(
        &path,
        &scope,
        group(&scope, &key, &request_a),
        vec![parked(&entered), parked(&entered)],
        &entered,
        2,
    );
    until_leases_lapse(&path, &key).await;

    // Staged runners serve the offered-resolution asks exactly as in the
    // green path; the mutant answers only the *re-resolution* asks — the
    // routing questions about the retained children — with impostor-bound
    // runners.
    let impostor_runs = Arc::new(AtomicUsize::new(0));
    let impostor_saw: Arc<Mutex<Vec<String>>> = Arc::default();
    let spy_b = Arc::new(SpyExecutors::reoffering_by_key(
        &impostor_runs,
        &impostor_saw,
    ));
    let world_b = world(&path, &spy_b).await;
    let scoped_b = world_b.scoped(scope.clone()).expect("scope");
    let mut handle = open(
        &scoped_b,
        group(&scope, &key, &request_b),
        &spy_b,
        vec![
            impostor(&impostor_runs, &impostor_saw),
            impostor(&impostor_runs, &impostor_saw),
        ],
    )
    .await;

    // The green path's assertions invert. First the leak itself: an
    // impostor-bound runner executes each retained child.
    tokio::time::timeout(AWAIT_BUDGET, async {
        while impostor_runs.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect(
        "under the key-only mutant an impostor-bound runner runs both \
         retained children — the oracle's `impostor ran 0 times` assertion \
         fails here",
    );
    // Then its consequence: settlements arrive for children nobody here was
    // authorized to run.
    let settlement = tokio::time::timeout(
        Duration::from_millis(2 * LEASE.as_millis() as u64),
        scoped_b
            .controller()
            .await_next_settlement(&mut handle, CancellationToken::new()),
    )
    .await;
    assert!(
        settlement.is_ok(),
        "under the key-only mutant a retained child settles — the oracle's \
         `no settlement arrives` assertion fails here"
    );
    let request_a = serde_json::to_string(&request_a).expect("the retained request serializes");
    assert_eq!(
        impostor_saw.lock_recover().clone(),
        vec![request_a.clone(), request_a],
        "the impostor-bound runners were handed the retained requests — the \
         oracle's `impostor was never handed the retained request` assertion \
         fails here"
    );
    // The routing question is the same as the honest path asks; it is the
    // answer that leaks.
    assert_eq!(
        spy_b.asked(),
        vec![child_key(&key, 0), child_key(&key, 1)],
        "the mutant answers the same asks — matching on the key alone is the \
         leak, not the asking"
    );

    scoped_b
        .controller()
        .close_effect_group(handle, RUN)
        .await
        .expect("the caller closes");
}

// =============================================================================
// Fixtures
// =============================================================================

fn child_key(group_key: &str, position: usize) -> String {
    format!("{group_key}:child:{position}")
}

fn child_envelope(
    scope: &ExecutionScope,
    group_key: &str,
    position: usize,
    request: &ToolChildRequest,
) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), child_key(group_key, position))
                .expect("valid group-child address"),
            RuntimeAttribution::none(),
            "effect",
        ),
        RuntimeEffectCommand::ToolInvocation {
            request: Box::new(request.clone()),
        },
    )
}

/// The same group header over children built from one request, at positions
/// that differ only in replay key.
fn group(scope: &ExecutionScope, key: &str, request: &ToolChildRequest) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), format!("{key}:group")).expect("valid group address"),
            RuntimeAttribution::none(),
            "group",
        ),
        key,
        (0..2)
            .map(|position| child_envelope(scope, key, position, request))
            .collect(),
        GroupWakePolicy::All,
        RUN,
    )
    .expect("a group with at least one child assembles")
}

/// What a spy resolver answers for a child it has no staged runner for.
enum SpyAnswer {
    /// "This host cannot run that request" — the `NoExecutor` case.
    Refuse,
    /// Runs the retained request, records it, and settles.
    Capture(Arc<Mutex<Vec<ToolChildRequest>>>),
    /// The injected mutant (FIG-3429): answers the routing question by replay
    /// key alone — an impostor-bound runner for whatever key the driver asks
    /// about, regardless of which request the retained child carries. Under
    /// this answer every oracle assertion inverts, which is the red-proof the
    /// oracle owes before it is trusted.
    ReofferByKey {
        runs: Arc<AtomicUsize>,
        saw: Arc<Mutex<Vec<String>>>,
    },
}

/// One opener's resolver, instrumented.
///
/// A law's own open-time runners are staged under the children's replay keys
/// and consumed without touching the ask log — so the ask log contains only
/// the routing questions the driver asked about children the staging did not
/// cover, which is the list the differential asserts on.
struct SpyExecutors {
    staged: Mutex<HashMap<String, RuntimeEffectLocalExecutor<'static>>>,
    asked: Mutex<Vec<String>>,
    answer: SpyAnswer,
}

impl SpyExecutors {
    fn refusing() -> Self {
        Self {
            staged: Mutex::new(HashMap::new()),
            asked: Mutex::new(Vec::new()),
            answer: SpyAnswer::Refuse,
        }
    }

    fn capturing(captured: &Arc<Mutex<Vec<ToolChildRequest>>>) -> Self {
        Self {
            staged: Mutex::new(HashMap::new()),
            asked: Mutex::new(Vec::new()),
            answer: SpyAnswer::Capture(Arc::clone(captured)),
        }
    }

    /// The mutant resolver: "reuses the offered runner" for any key the driver
    /// re-resolves — the key-only match the retained-envelope binding exists to
    /// stop mattering. Each re-resolution ask gets a fresh impostor-bound
    /// runner, so a leak is observable per retained child.
    fn reoffering_by_key(runs: &Arc<AtomicUsize>, saw: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            staged: Mutex::new(HashMap::new()),
            asked: Mutex::new(Vec::new()),
            answer: SpyAnswer::ReofferByKey {
                runs: Arc::clone(runs),
                saw: Arc::clone(saw),
            },
        }
    }

    fn asked(&self) -> Vec<String> {
        let mut keys = self.asked.lock_recover().clone();
        keys.sort();
        keys
    }
}

impl GroupExecutors for SpyExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let replay_key = envelope.invocation.replay_key().to_string();
        // A staged runner is taken by the resolution that staged it, without
        // touching the ask log — exactly as the suite's staging table behaves.
        if let Some(executor) = self.staged.lock_recover().remove(&replay_key) {
            return Some(executor);
        }
        self.asked.lock_recover().push(replay_key);
        match &self.answer {
            SpyAnswer::Refuse => None,
            SpyAnswer::Capture(captured) => Some(capturing(captured)),
            SpyAnswer::ReofferByKey { runs, saw } => Some(impostor(runs, saw)),
        }
    }
}

/// A runner that reports it started and then parks, so the crashed process
/// leaves claimed, journaled children behind.
fn parked(entered: &Arc<AtomicUsize>) -> RuntimeEffectLocalExecutor<'static> {
    let entered = Arc::clone(entered);
    RuntimeEffectLocalExecutor::testing(move |_| async move {
        entered.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("a parked child is never polled to completion")
    })
}

/// A runner bound to the *offered* request that must never run — it counts
/// invocations and records the request it was handed, so the differential can
/// name what leaked rather than only that something did.
fn impostor(
    runs: &Arc<AtomicUsize>,
    saw: &Arc<Mutex<Vec<String>>>,
) -> RuntimeEffectLocalExecutor<'static> {
    let runs = Arc::clone(runs);
    let saw = Arc::clone(saw);
    RuntimeEffectLocalExecutor::testing(move |envelope| {
        let runs = Arc::clone(&runs);
        let saw = Arc::clone(&saw);
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            if let RuntimeEffectCommand::ToolInvocation { request } = &envelope.command {
                saw.lock_recover()
                    .push(serde_json::to_string(request).expect("the request serializes"));
            }
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!("impostor"),
            })
        }
    })
}

/// A runner that records the retained request it was handed and settles.
fn capturing(saw: &Arc<Mutex<Vec<ToolChildRequest>>>) -> RuntimeEffectLocalExecutor<'static> {
    let saw = Arc::clone(saw);
    RuntimeEffectLocalExecutor::testing(move |envelope| {
        let saw = Arc::clone(&saw);
        async move {
            if let RuntimeEffectCommand::ToolInvocation { request } = &envelope.command {
                saw.lock_recover().push((**request).clone());
            }
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({ "settled": true }),
            })
        }
    })
}

/// One host over the shared journal, wired to the given resolver.
async fn world(path: &Path, spy: &Arc<SpyExecutors>) -> Arc<SqliteEffectHost> {
    let host = SqliteEffectHost::open_with_options(
        path,
        SqliteEffectReplayOptions {
            lease_timings: LeaseTimings::new(LEASE, LEASE / 3)
                .expect("a lease at least three renew intervals wide"),
        },
    )
    .await
    .expect("the SQLite effect host opens");
    host.register_group_executors(Arc::clone(spy) as Arc<dyn GroupExecutors>)
        .expect("a freshly opened host has no resolver yet");
    Arc::new(host)
}

/// Opens the group, waits for `expected` children to be in flight, and then
/// destroys the runtime — the crash that leaves claimed children owned by
/// nobody.
///
/// The host is opened inside a runtime of its own, because the point of the
/// phase is that the runtime — and every task and connection it owns — dies.
fn crash_opening(
    path: &Path,
    scope: &ExecutionScope,
    group: RuntimeEffectGroup,
    executors: Vec<RuntimeEffectLocalExecutor<'static>>,
    entered: &Arc<AtomicUsize>,
    expected: usize,
) {
    let path = path.to_path_buf();
    let scope = scope.clone();
    let entered = Arc::clone(entered);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the crashing process gets a runtime of its own");
        runtime.block_on(async move {
            let spy = Arc::new(SpyExecutors::refusing());
            assert_eq!(
                group.children().len(),
                executors.len(),
                "a test stages one executor per child"
            );
            for (child, executor) in group.children().iter().zip(executors) {
                spy.staged
                    .lock_recover()
                    .insert(child.invocation.replay_key().to_string(), executor);
            }
            let world = world(&path, &spy).await;
            let scoped = world.scoped(scope).expect("scope");
            let _handle = scoped
                .controller()
                .open_effect_group(group)
                .await
                .expect("the group opens");
            while entered.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(POLL).await;
            }
        });
        drop(runtime);
    })
    .join()
    .expect("the crashing process runs its phase before dying");
}

/// Waits until the crashed process's claims have lapsed, without running
/// anything: a refusing drain asks the same question and answers `NoExecutor`,
/// which writes nothing.
async fn until_leases_lapse(path: &Path, group_key: &str) {
    let spy = Arc::new(SpyExecutors::refusing());
    let probe = world(path, &spy).await;
    let drain = probe.group_drain();
    drain_until_no_live_lease(&drain, group_key).await;
}

/// Drains until a pass finds nothing left under a live lease, and returns
/// that pass.
async fn drain_until_no_live_lease(
    drain: &Arc<dyn StoreEffectGroupDrain>,
    group_key: &str,
) -> lash_core::GroupDrainReport {
    tokio::time::timeout(AWAIT_BUDGET, async {
        loop {
            let report = drain
                .drain_group(group_key, &CancellationToken::new())
                .await
                .expect("a drain pass over the group runs");
            if report
                .children
                .iter()
                .all(|child| !matches!(child.outcome, ChildDrainOutcome::LeaseLive { .. }))
            {
                return report;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the dead process's claims lapse")
}

/// Opens a group with one staged runner per child — or none, when the caller
/// already staged them.
async fn open(
    scoped: &ScopedEffectController<'_>,
    group: RuntimeEffectGroup,
    spy: &Arc<SpyExecutors>,
    executors: Vec<RuntimeEffectLocalExecutor<'static>>,
) -> EffectGroupHandle {
    assert!(
        executors.is_empty() || executors.len() == group.children().len(),
        "a test stages one executor per child"
    );
    for (child, executor) in group.children().iter().zip(executors) {
        spy.staged
            .lock_recover()
            .insert(child.invocation.replay_key().to_string(), executor);
    }
    scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("the group opens")
}
