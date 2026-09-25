//! Possession conservation oracle (FIG-3429, ADR 0099 §1).
//!
//! Run-local possession is the set of process ids a `RuntimeExecutionContext`
//! may drive without borrowing session-level visibility: `authorize_handle`
//! answers `RunLocalPossession` for ids in `started_process_ids` before it ever
//! asks the observer edges. Possession is *conferred*, not ambient — a start
//! that realizes a child must land the id in the realizing run's set:
//!
//! - `start_child_process` records possession the moment the registry row
//!   lands (the in-session direct start);
//! - `record_processes_started_by_intents` lifts the realized handle out of a
//!   settled `Executed { StartProcess }` intent outcome (the dispatch-channel
//!   start, which holds no context of its own);
//! - `restore_started_process_ids` carries the same set across a segment
//!   handover, so the next context of the same run keeps its children.
//!
//! The conservation law: **after every sim step, every realized process in the
//! registry is possessed by exactly one live opener — the opener that realized
//! it — and no opener possesses an id nothing realized.** Two of the three
//! failure shapes are the C1 finding in miniature: a realized child possessed
//! by *zero* openers is unreachable to the run that owns it (the
//! orchestrating-starts leak), and a child possessed by *two* openers is an
//! authority leak outright. The third — possession of a never-realized id —
//! is the phantom half of the same boundary.
//!
//! The oracle drives a two-opener world over one registry and checks the
//! invariant after every step: direct starts, intent starts, refused and
//! protocol-refused intent outcomes, an executed non-start intent whose result
//! echoes a live handle (a result is data, not authority — it must grant
//! nothing), a registry row no run realized, a segment handover, and a
//! crash/recover that restores the handover snapshot into the opener's next
//! context.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::testing::{MockSessionManager, TestExecutionContextBuilder};
use lash_core::tool_dispatch::ToolDispatchOutcome;
use lash_core::{
    EffectOpener, OnParentEnd, ParentScope, PluginOptions, ProcessExecutionEnvSpec, ProcessId,
    ProcessInput, ProcessLifecyclePolicy, ProcessListFilter, ProcessOriginator, ProcessProvenance,
    ProcessRef, ProcessRegistration, ProcessStartRequest, RecoveryContract,
    RuntimeExecutionContext, SessionId, SessionPolicy, ToolCallOutcome, ToolCallOutput,
    ToolCallRecord, ToolIntentExecutionOutcome, ToolIntentIdentity, ToolIntentKind,
    ToolIntentRefusalReason, ToolIntents, TurnBudget, TurnId,
};

/// One live opener: a session's run-local execution context plus the counters
/// that keep its tool-call and intent identities distinct across steps.
struct Opener {
    session: SessionId,
    context: RuntimeExecutionContext<'static>,
    next_call: u64,
    next_intent: u32,
}

/// The two-opener world: one registry, one context per live opener, and the
/// world's own ledger of what each start channel actually realized.
struct PossessionWorld {
    /// The memory backend (ADR 0102) the world's contexts journal on and
    /// whose registry the session host's process routes write.
    backend: lash_sqlite_store::SqliteBackend,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    host: Arc<MockSessionManager>,
    openers: BTreeMap<&'static str, Opener>,
    /// Registry rows a possession-conferring start channel realized, keyed to
    /// the opener that realized them.
    realized: BTreeMap<ProcessId, &'static str>,
    /// Registry rows no run realized — observer/external rows. They are
    /// session-visible, never run-local: legitimately possessed by no opener.
    registered_only: BTreeSet<ProcessId>,
}

impl PossessionWorld {
    #[expect(
        clippy::expect_used,
        reason = "test fixture: a memory backend that fails to open aborts the law"
    )]
    async fn new() -> Self {
        let backend = lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open a memory backend");
        let registry = lash_core::Backend::from(backend.clone()).process_registry();
        Self {
            host: Arc::new(
                MockSessionManager::default().with_process_registry(Arc::clone(&registry)),
            ),
            backend,
            registry,
            openers: BTreeMap::new(),
            realized: BTreeMap::new(),
            registered_only: BTreeSet::new(),
        }
    }

    fn context_for(&self, session: &SessionId) -> RuntimeExecutionContext<'static> {
        TestExecutionContextBuilder::for_backend(&self.backend.clone().into())
            .session_id(session.clone())
            .shared_session_host(self.host.clone())
            .processes(self.host.clone())
            .build()
            .into_runtime()
    }

    /// World creation is itself a step: an empty world must already satisfy
    /// the law before any opener acts.
    async fn add_opener(&mut self, name: &'static str, session: &str) {
        let session = SessionId::from(session);
        let context = self.context_for(&session);
        self.openers.insert(
            name,
            Opener {
                session,
                context,
                next_call: 0,
                next_intent: 0,
            },
        );
        self.assert_conservation(&format!("add opener {name}"))
            .await;
    }

    fn opener(&mut self, name: &str) -> &mut Opener {
        self.openers
            .get_mut(name)
            .unwrap_or_else(|| panic!("unknown opener {name}"))
    }

    /// A direct in-session start: `start_child_process` registers the row and
    /// records possession in the same settled step.
    async fn realize_direct_start(&mut self, opener_name: &'static str, child: &str) {
        let opener = self.opener(opener_name);
        let child_id = ProcessId::from(format!("{opener_name}-{child}"));
        let reply = opener
            .context
            .start_child_process(
                ProcessStartRequest::new(
                    child_id.clone(),
                    ProcessInput::Engine {
                        kind: "sim-child".to_string(),
                        payload: serde_json::Value::Null,
                    },
                    RecoveryContract::Rerunnable,
                    ProcessOriginator::Session {
                        session_id: opener.session.clone(),
                        agent_frame_id: None,
                    },
                    ProcessLifecyclePolicy::new(
                        ParentScope::Owned(EffectOpener::turn(
                            opener.session.clone(),
                            TurnId::from(format!("turn-{opener_name}")),
                        )),
                        OnParentEnd::Cancel,
                    ),
                ),
                "engine",
                Some(child.to_string()),
            )
            .await;
        assert!(
            matches!(reply.output.outcome, ToolCallOutcome::Success(_)),
            "direct start of {child_id} under {opener_name} failed: {:?}",
            reply.output
        );
        self.realized.insert(child_id, opener_name);
        self.assert_conservation(&format!("{opener_name} direct-start {child}"))
            .await;
    }

    /// An intent-channel start: the declaration's replay key *is* the child id
    /// (`ProcessId::from_intent_identity`), the intent's own execution lands
    /// the registry row, and the settled `Executed { StartProcess }` outcome
    /// carries the realized handle home to the run's possession set.
    #[expect(
        clippy::expect_used,
        reason = "test fixture: a call that fails to present aborts the law"
    )]
    async fn realize_intent_start(&mut self, opener_name: &'static str, child: &str) {
        let (call_id, identity, child_id) = {
            let opener = self.opener(opener_name);
            let (call_id, identity) = opener.next_intent_identity(child);
            let child_id = ProcessId::from_intent_identity(&identity);
            (call_id, identity, child_id)
        };
        let record = self
            .registry
            .register_process(self.realized_registration(&child_id, opener_name))
            .await
            .unwrap_or_else(|err| panic!("register realized child {child_id}: {err}"));
        let handle =
            RuntimeExecutionContext::process_handle_json(&ProcessRef::from_record(&record));
        let opener = self.opener(opener_name);
        let outcome = settled_outcome(
            &call_id,
            handle.clone(),
            vec![ToolIntentExecutionOutcome::Executed {
                identity,
                kind: ToolIntentKind::StartProcess,
                result: handle,
            }],
        );
        opener
            .context
            .complete_tool_call(call_id, None, outcome)
            .await
            .expect("the call presents");
        self.realized.insert(child_id, opener_name);
        self.assert_conservation(&format!("{opener_name} intent-start {child}"))
            .await;
    }

    /// A refused start intent: nothing was realized, so nothing may be
    /// possessed. `child` is never registered — possession of it would be
    /// phantom on top of phantom.
    #[expect(
        clippy::expect_used,
        reason = "test fixture: a call that fails to present aborts the law"
    )]
    async fn settle_refused_start(&mut self, opener_name: &'static str, child: &str) {
        let opener = self.opener(opener_name);
        let (call_id, identity) = opener.next_intent_identity(child);
        let intent_index = identity.intent_index;
        let outcome = settled_outcome(
            &call_id,
            serde_json::json!({"refused": true}),
            vec![ToolIntentExecutionOutcome::Refused {
                identity: Some(identity),
                intent_index,
                kind: ToolIntentKind::StartProcess,
                refusal: ToolIntentRefusalReason::MissingToolCallId,
            }],
        );
        opener
            .context
            .complete_tool_call(call_id, None, outcome)
            .await
            .expect("the call presents");
        self.assert_conservation(&format!("{opener_name} refused-start {child}"))
            .await;
    }

    /// A batch-level protocol refusal carries no declaration at all.
    #[expect(
        clippy::expect_used,
        reason = "test fixture: a call that fails to present aborts the law"
    )]
    async fn settle_protocol_refused(&mut self, opener_name: &'static str) {
        let opener = self.opener(opener_name);
        opener.next_call += 1;
        let call_id = format!("call-{opener_name}-{}", opener.next_call);
        let outcome = settled_outcome(
            &call_id,
            serde_json::json!({"refused": true}),
            vec![ToolIntentExecutionOutcome::ProtocolRefused {
                refusal: ToolIntentRefusalReason::MissingToolCallId,
            }],
        );
        opener
            .context
            .complete_tool_call(call_id, None, outcome)
            .await
            .expect("the call presents");
        self.assert_conservation(&format!("{opener_name} protocol-refused"))
            .await;
    }

    /// An executed non-start intent whose *result* echoes a live child's
    /// handle. Signal replies routinely carry the target's handle back — the
    /// result is data, not authority, and the kind guard is what stops an echo
    /// from conferring possession of a process the settling run never started.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn settle_signal_echoing_handle(&mut self, opener_name: &'static str, victim: &str) {
        let victim_id = ProcessId::from(victim);
        let victim_record = self
            .registry
            .get_process(&victim_id)
            .await
            .expect("registry read")
            .unwrap_or_else(|| panic!("echo victim {victim_id} is not registered"));
        let echoed =
            RuntimeExecutionContext::process_handle_json(&ProcessRef::from_record(&victim_record));
        let opener = self.opener(opener_name);
        let (call_id, identity) = opener.next_intent_identity("signal");
        let outcome = settled_outcome(
            &call_id,
            serde_json::json!({"signaled": true}),
            vec![ToolIntentExecutionOutcome::Executed {
                identity,
                kind: ToolIntentKind::SignalProcess,
                result: echoed,
            }],
        );
        opener
            .context
            .complete_tool_call(call_id, None, outcome)
            .await
            .expect("the call presents");
        self.assert_conservation(&format!("{opener_name} signal-echo on {victim}"))
            .await;
    }

    /// A registry row no run realized: durable and session-visible, but never
    /// run-local. Possession of it by any opener is a phantom.
    async fn register_observed_only(&mut self, label: &str) {
        let process_id = ProcessId::from(label);
        self.registry
            .register_process(ProcessRegistration::new(
                process_id.clone(),
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
                ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
            ))
            .await
            .unwrap_or_else(|err| panic!("register observed-only row {process_id}: {err}"));
        self.registered_only.insert(process_id);
        self.assert_conservation(&format!("observed-only {label}"))
            .await;
    }

    /// A segment handover: the run's next context restores the possession
    /// snapshot the boundary carried (`restore_started_process_ids`), and the
    /// prior context retires with it. Possession stays exactly-one across the
    /// swap.
    async fn handover(&mut self, opener_name: &'static str) {
        let (session, snapshot) = {
            let opener = self.opener(opener_name);
            (opener.session.clone(), opener.context.started_process_ids())
        };
        let context = self.context_for(&session);
        context.restore_started_process_ids(&snapshot);
        self.opener(opener_name).context = context;
        self.assert_conservation(&format!("{opener_name} handover"))
            .await;
    }

    /// A crash followed by recovery: the dead context is gone, the handover
    /// snapshot survives on the journal side, and the recovered incarnation's
    /// fresh context restores it before serving anything.
    async fn crash_and_recover(&mut self, opener_name: &'static str) {
        self.handover(opener_name).await;
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    fn realized_registration(
        &self,
        child_id: &ProcessId,
        opener_name: &'static str,
    ) -> ProcessRegistration {
        let session = &self.openers[opener_name].session;
        // Engine rows must name the captured environment they run under.
        let env_ref = ProcessExecutionEnvSpec::new(
            PluginOptions::default(),
            SessionPolicy::new(TurnBudget::Unbounded),
        )
        .stable_ref()
        .expect("env spec content-addresses");
        ProcessRegistration::new(
            child_id.clone(),
            ProcessInput::Engine {
                kind: "sim-child".to_string(),
                payload: serde_json::Value::Null,
            },
            RecoveryContract::Rerunnable,
            ProcessProvenance::new(ProcessOriginator::Session {
                session_id: session.clone(),
                agent_frame_id: None,
            }),
            ProcessLifecyclePolicy::new(
                ParentScope::Owned(EffectOpener::turn(
                    session.clone(),
                    TurnId::from(format!("turn-{opener_name}")),
                )),
                OnParentEnd::Cancel,
            ),
        )
        .with_execution_env_ref(Some(env_ref))
    }

    /// The conservation law, evaluated after every step:
    ///
    /// 1. possession sets are pairwise disjoint across live openers;
    /// 2. every realized process is possessed by exactly one opener — the
    ///    opener that realized it;
    /// 3. nothing is possessed that was never realized (no phantoms);
    /// 4. rows no start channel realized are possessed by nobody;
    /// 5. the registry actually holds every row the ledger claims realized —
    ///    and holds nothing the ledger cannot account for.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn assert_conservation(&self, step: &str) {
        let mut owners: BTreeMap<ProcessId, Vec<&'static str>> = BTreeMap::new();
        for (name, opener) in &self.openers {
            for process_id in opener.context.started_process_ids() {
                owners.entry(process_id).or_default().push(name);
            }
        }
        for (process_id, names) in &owners {
            assert_eq!(
                names.len(),
                1,
                "[{step}] {process_id} is possessed by {} openers {names:?}; \
                 exactly one live opener may hold run-local possession",
                names.len()
            );
        }
        for (process_id, realized_by) in &self.realized {
            let names = owners.get(process_id).unwrap_or_else(|| {
                panic!(
                    "[{step}] realized process {process_id} is possessed by no opener; \
                     the run that started it ({realized_by}) cannot reach it"
                )
            });
            assert_eq!(
                names.as_slice(),
                &[*realized_by],
                "[{step}] {process_id} is possessed by {names:?}, \
                 not the opener that realized it ({realized_by})"
            );
        }
        for process_id in owners.keys() {
            assert!(
                self.realized.contains_key(process_id),
                "[{step}] an opener possesses {process_id}, which no start channel realized"
            );
        }
        for process_id in &self.registered_only {
            assert!(
                !owners.contains_key(process_id),
                "[{step}] {process_id} was never realized by any run but is possessed"
            );
        }
        for process_id in self.realized.keys() {
            assert!(
                self.registry
                    .get_process(process_id)
                    .await
                    .expect("registry read")
                    .is_some(),
                "[{step}] ledger claims {process_id} realized but the registry has no row"
            );
        }
        let registry_rows: BTreeSet<ProcessId> = self
            .registry
            .list_processes(&ProcessListFilter::default())
            .await
            .expect("registry list")
            .into_iter()
            .map(|record| record.id)
            .collect();
        for process_id in &registry_rows {
            assert!(
                self.realized.contains_key(process_id) || self.registered_only.contains(process_id),
                "[{step}] registry row {process_id} is accounted for by neither \
                 the realized nor the observed-only ledger"
            );
        }
    }
}

impl Opener {
    fn next_intent_identity(&mut self, child: &str) -> (String, ToolIntentIdentity) {
        self.next_call += 1;
        self.next_intent += 1;
        let call_id = format!("call-{}-{}", self.session, self.next_call);
        let identity = ToolIntentIdentity {
            session_id: self.session.clone(),
            execution_scope_id: format!("turn-{}", self.session),
            tool_call_id: call_id.clone(),
            intent_index: self.next_intent,
            replay_key: format!("{}-{}", self.session, child),
            minting_emission_replay_key: None,
        };
        (call_id, identity)
    }
}

fn settled_outcome(
    call_id: &str,
    output: serde_json::Value,
    intent_outcomes: Vec<ToolIntentExecutionOutcome>,
) -> ToolDispatchOutcome {
    ToolDispatchOutcome {
        record: ToolCallRecord {
            call_id: Some(call_id.to_string()),
            tool: "sim-tool".to_string(),
            args: serde_json::Value::Null,
            output: ToolCallOutput::success(output),
            duration_ms: 0,
        },
        attempts: Vec::new(),
        intents: ToolIntents::default(),
        intent_outcomes,
        captures: Vec::new(),
        triggers: Vec::new(),
    }
}

/// The conservation scenario: every step is followed by the full invariant
/// inside its helper, so the sequence below reads as the workload itself.
#[tokio::test]
async fn realized_processes_are_possessed_by_exactly_one_opener_after_every_step() {
    let mut world = PossessionWorld::new().await;
    world.add_opener("A", "session-a").await;
    world.add_opener("B", "session-b").await;

    // Both possession-conferring channels realize children under A while B is
    // live and empty.
    world.realize_direct_start("A", "direct-1").await;
    world.realize_intent_start("A", "intent-1").await;

    // B realizes its own child; the sets must stay disjoint.
    world.realize_intent_start("B", "intent-1").await;

    // Outcomes that realized nothing confer nothing — even one whose result
    // echoes a live handle A owns.
    world.settle_refused_start("B", "ghost").await;
    world.settle_protocol_refused("B").await;
    world
        .settle_signal_echoing_handle("B", "session-b-intent-1")
        .await;
    world
        .settle_signal_echoing_handle("B", "session-a-intent-1")
        .await;

    // Durable rows outside every run's possession stay unpossessed.
    world.register_observed_only("host-observed").await;

    // Possession survives segment boundaries and crash/recover exactly once.
    world.handover("A").await;
    world.crash_and_recover("B").await;

    // A recovered opener still confers possession on new realizations.
    world.realize_intent_start("B", "intent-2").await;
    world.realize_direct_start("A", "direct-2").await;
}
