//! L19: overlapping plugin state never depends on unrecorded work (K10, Q5).
//!
//! Each law drives the two halves an engine runs: the recorded body, whose
//! outcome carries its resolutions, and the frame that publishes that outcome
//! once the engine returns it. A cold replay is a session rebuilt from a
//! checkpoint that is handed the recorded outcomes again, as an engine serves
//! them from its journal.
use super::*;
use crate::plugin::{PluginDeclaration, PluginSessionRequest, PluginSpec, StaticPluginFactory};
use std::sync::atomic::{AtomicUsize, Ordering};

const LEDGER: &str = "ledger";
const OTHER: &str = "other";

/// A host with the `ledger` plugin, whose reducers count their calls in
/// `reducer_calls`, and the reducer-less `other` plugin.
fn host(reducer_calls: &Arc<AtomicUsize>) -> crate::PluginHost {
    let append_calls = Arc::clone(reducer_calls);
    let add_calls = Arc::clone(reducer_calls);
    let ledger = PluginSpec::new()
        .with_state_reducer(
            "append",
            Arc::new(move |reduction: StateReduction<'_>| {
                append_calls.fetch_add(1, Ordering::SeqCst);
                let current = reduction.current.and_then(Value::as_str).unwrap_or("");
                let suffix = reduction.input.as_str().unwrap_or("");
                Ok(Some(Value::String(format!("{current}{suffix}"))))
            }),
        )
        .with_state_reducer(
            "add",
            Arc::new(move |reduction: StateReduction<'_>| {
                add_calls.fetch_add(1, Ordering::SeqCst);
                let current = reduction.current.and_then(Value::as_u64).unwrap_or(0);
                let step = reduction.input.as_u64().unwrap_or(0);
                Ok(Some(serde_json::json!(current + step)))
            }),
        )
        .with_state_reducer(
            "overdraw",
            Arc::new(|_: StateReduction<'_>| {
                Err(HookCause {
                    error_type: "ledger.overdrawn".into(),
                    error_version: std::num::NonZeroU32::MIN,
                    payload: serde_json::json!({"balance": 0}),
                })
            }),
        )
        .with_state_reducer(
            "panics",
            Arc::new(|_: StateReduction<'_>| panic!("a reducer bug")),
        );
    crate::PluginHost::new(vec![
        Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial(LEDGER),
            ledger,
        )),
        Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial(OTHER),
            PluginSpec::new(),
        )),
    ])
}

fn session(host: &crate::PluginHost, snapshot: Option<&PluginState>) -> Arc<crate::PluginSession> {
    let request = match snapshot {
        Some(snapshot) => {
            PluginSessionRequest::rematerialization("state-owner", snapshot, Default::default())
        }
        None => PluginSessionRequest::creation("state-owner", Default::default()),
    };
    host.isolated_registry().build_session(request).unwrap()
}

fn address(step: &str) -> crate::EffectAddress {
    crate::EffectAddress::new(crate::ExecutionScope::turn("state-owner", "run"), step).unwrap()
}

fn revision(plugin: &str) -> lash_core_store::store::plugin_writers::PluginRevision {
    lash_core_store::store::plugin_writers::PluginRevision::new(
        plugin,
        crate::plugin::BehaviorRevision::ONE,
    )
}

/// A tool body's commands for `plugin`, from call `call`'s first attempt.
fn tool_commands(plugin: &str, call: &str, commands: StateCommands) -> Proposal {
    Proposal::for_tool(
        revision(plugin),
        StateCommandOrigin::ToolAttempt {
            call_id: crate::ToolCallId::fixture(call),
            attempt: lash_core_store::tool_run::AttemptOrdinal::FIRST,
        },
        commands,
    )
}

/// Run the recorded body `step`, which proposes `proposals`: its outcome
/// as an engine records it, before the engine returns it.
async fn record(
    session: &Arc<crate::PluginSession>,
    step: &str,
    proposals: Vec<Proposal>,
) -> RuntimeEffectOutcome {
    let body_session = Arc::clone(session);
    record_effect(
        Arc::clone(session),
        RuntimeEffectKind::LanguageRuntimeValue,
        address(step),
        async move {
            propose_all(&body_session, proposals)?;
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue { value: Value::Null })
        },
    )
    .await
    .unwrap()
}

/// The engine returned `outcome`: publish it.
fn publish(
    session: &Arc<crate::PluginSession>,
    step: &str,
    outcome: RuntimeEffectOutcome,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    EffectPublication::begin(Arc::clone(session), address(step)).publish(outcome)
}

/// The outcome's journal bytes and back: what a cold replay is served.
fn journaled(outcome: &RuntimeEffectOutcome) -> RuntimeEffectOutcome {
    rmp_serde::from_slice(&rmp_serde::to_vec_named(outcome).unwrap()).unwrap()
}

fn resolutions(outcome: &RuntimeEffectOutcome) -> &[StateResolution] {
    let RuntimeEffectOutcome::PluginState { state, .. } = outcome else {
        panic!("the outcome carries its resolutions: {outcome:?}");
    };
    &state.resolutions
}

fn value(session: &crate::PluginSession, plugin: &str, key: &str) -> Option<Value> {
    session.export_state().plugins[plugin]
        .values
        .get(key)
        .cloned()
}

/// The three overlaps the law covers: what A writes and what B, which reads
/// the published state when its body runs, writes.
#[derive(Clone, Copy, Debug)]
enum Overlap {
    SameKey,
    DisjointKeys,
    DifferentNamespaces,
}

impl Overlap {
    /// A's namespace and key, then B's.
    fn keys(self) -> ((&'static str, &'static str), (&'static str, &'static str)) {
        match self {
            Self::SameKey => ((LEDGER, "k"), (LEDGER, "k")),
            Self::DisjointKeys => ((LEDGER, "a"), (LEDGER, "b")),
            Self::DifferentNamespaces => ((OTHER, "a"), (LEDGER, "b")),
        }
    }
}

/// A holds an unrecorded command and is cancelled; B, running beside it,
/// becomes durable. B never saw A's command, and a cold replay of B's
/// recorded outcome installs B alone, running no body and no reducer.
#[tokio::test]
async fn an_unrecorded_command_never_reaches_a_durable_sibling() {
    for overlap in [
        Overlap::SameKey,
        Overlap::DisjointKeys,
        Overlap::DifferentNamespaces,
    ] {
        let reducer_calls = Arc::new(AtomicUsize::new(0));
        let host = host(&reducer_calls);
        let live = session(&host, None);
        let base = live.export_state();
        let ((a_plugin, a_key), (b_plugin, b_key)) = overlap.keys();
        let bodies = Arc::new(AtomicUsize::new(0));

        let (proposed, a_proposed) = tokio::sync::oneshot::channel();
        let a_session = Arc::clone(&live);
        let a_bodies = Arc::clone(&bodies);
        let mut a = Box::pin(record_effect(
            Arc::clone(&live),
            RuntimeEffectKind::LanguageRuntimeValue,
            address("a"),
            async move {
                a_bodies.fetch_add(1, Ordering::SeqCst);
                propose(
                    &a_session,
                    tool_commands(
                        a_plugin,
                        "a",
                        StateCommands::new().set(a_key, serde_json::json!("from-a")),
                    ),
                )?;
                proposed.send(()).unwrap();
                std::future::pending().await
            },
        ));
        tokio::select! {
            _ = &mut a => panic!("A's body holds its command unrecorded"),
            _ = a_proposed => {}
        }

        let view = PluginStateView::bind(live.owner(), a_plugin, Arc::clone(&live.state));
        let b_session = Arc::clone(&live);
        let b_bodies = Arc::clone(&bodies);
        let b = record_effect(
            Arc::clone(&live),
            RuntimeEffectKind::LanguageRuntimeValue,
            address("b"),
            async move {
                b_bodies.fetch_add(1, Ordering::SeqCst);
                let seen = view.get(a_key);
                propose(
                    &b_session,
                    tool_commands(
                        b_plugin,
                        "b",
                        StateCommands::new().set(b_key, serde_json::json!({ "saw": seen })),
                    ),
                )?;
                Ok(RuntimeEffectOutcome::LanguageRuntimeValue { value: Value::Null })
            },
        )
        .await
        .unwrap();
        publish(&live, "b", b.clone()).unwrap();
        drop(a);

        let expected = serde_json::json!({"saw": null});
        assert_eq!(
            value(&live, b_plugin, b_key),
            Some(expected.clone()),
            "{overlap:?}"
        );
        if a_key != b_key || a_plugin != b_plugin {
            assert_eq!(value(&live, a_plugin, a_key), None, "{overlap:?}");
        }

        let cold = session(&host, Some(&base));
        publish(&cold, "b", journaled(&b)).unwrap();
        assert_eq!(value(&cold, b_plugin, b_key), Some(expected), "{overlap:?}");
        if a_key != b_key || a_plugin != b_plugin {
            assert_eq!(value(&cold, a_plugin, a_key), None, "{overlap:?}");
        }
        assert_eq!(cold.export_state(), live.export_state(), "{overlap:?}");
        assert_eq!(
            bodies.load(Ordering::SeqCst),
            2,
            "{overlap:?}: no body reran"
        );
        assert_eq!(reducer_calls.load(Ordering::SeqCst), 0, "{overlap:?}");
    }
}

/// A reduced publication stays private until the engine returns it: a
/// sibling of its namespace waits and then reduces against it, while a body
/// of another namespace, or one that proposes nothing, never waits.
#[tokio::test]
async fn a_proposal_is_invisible_until_its_outcome_returns() {
    let reducer_calls = Arc::new(AtomicUsize::new(0));
    let host = host(&reducer_calls);
    let live = session(&host, None);
    let a = record(
        &live,
        "a",
        vec![tool_commands(
            LEDGER,
            "a",
            StateCommands::new().apply("total", "add", serde_json::json!(2)),
        )],
    )
    .await;
    assert_eq!(
        value(&live, LEDGER, "total"),
        None,
        "a proposal before its acknowledgement is invisible"
    );

    let sibling_session = Arc::clone(&live);
    let mut sibling = crate::task::spawn(async move {
        record(
            &sibling_session,
            "b",
            vec![tool_commands(
                LEDGER,
                "b",
                StateCommands::new().apply("total", "add", serde_json::json!(3)),
            )],
        )
        .await
    });
    let other = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        record(
            &live,
            "c",
            vec![tool_commands(
                OTHER,
                "c",
                StateCommands::new().set("x", serde_json::json!(1)),
            )],
        ),
    )
    .await
    .expect("another namespace never waits");
    let stateless = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        record(&live, "d", Vec::new()),
    )
    .await
    .expect("a stateless body never waits");
    assert!(matches!(
        stateless,
        RuntimeEffectOutcome::LanguageRuntimeValue { .. }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut sibling)
            .await
            .is_err(),
        "the sibling's reduction waits for the unreturned publication"
    );

    publish(&live, "a", a.clone()).unwrap();
    let b = sibling.await.unwrap();
    publish(&live, "b", b.clone()).unwrap();
    publish(&live, "c", other).unwrap();
    assert_eq!(value(&live, LEDGER, "total"), Some(serde_json::json!(5)));
    assert_eq!(
        resolutions(&b)[0].predecessor,
        Some(resolutions(&a)[0].ordinal),
        "the sibling reduced against the published predecessor"
    );
    assert_eq!(reducer_calls.load(Ordering::SeqCst), 2);
}

/// Two noncommutative results publish in their recorded order; a replay that
/// consumes them in the opposite order installs the same state, and runs no
/// reducer.
#[tokio::test]
async fn noncommutative_results_replay_in_their_recorded_order() {
    let reducer_calls = Arc::new(AtomicUsize::new(0));
    let host = host(&reducer_calls);
    let live = session(&host, None);
    let base = live.export_state();
    let mut recorded = Vec::new();
    for (step, suffix) in [("first", "a"), ("second", "b")] {
        let outcome = record(
            &live,
            step,
            vec![tool_commands(
                LEDGER,
                step,
                StateCommands::new().apply("log", "append", serde_json::json!(suffix)),
            )],
        )
        .await;
        publish(&live, step, outcome.clone()).unwrap();
        recorded.push((step, journaled(&outcome)));
    }
    assert_eq!(value(&live, LEDGER, "log"), Some(serde_json::json!("ab")));
    assert_eq!(reducer_calls.load(Ordering::SeqCst), 2);

    let cold = session(&host, Some(&base));
    let (second_step, second) = recorded[1].clone();
    publish(&cold, second_step, second).unwrap();
    assert_eq!(
        value(&cold, LEDGER, "log"),
        None,
        "a resolution delivered ahead of its predecessor waits for it"
    );
    let (first_step, first) = recorded[0].clone();
    publish(&cold, first_step, first).unwrap();
    assert_eq!(value(&cold, LEDGER, "log"), Some(serde_json::json!("ab")));
    assert_eq!(cold.export_state(), live.export_state());
    assert_eq!(
        reducer_calls.load(Ordering::SeqCst),
        2,
        "replay ran no reducer"
    );
}

/// A checkpoint carries each namespace's applied frontier: an older result
/// delivered again after a newer one applies nothing.
#[tokio::test]
async fn a_checkpoint_never_reapplies_an_older_delivery() {
    let host = host(&Arc::default());
    let live = session(&host, None);
    let mut recorded = Vec::new();
    for (step, total) in [("older", 1), ("newer", 2)] {
        let outcome = record(
            &live,
            step,
            vec![tool_commands(
                LEDGER,
                step,
                StateCommands::new().set("total", serde_json::json!(total)),
            )],
        )
        .await;
        publish(&live, step, outcome.clone()).unwrap();
        recorded.push((step, journaled(&outcome)));
    }
    let checkpoint = live.export_state();
    assert_eq!(checkpoint.plugins[LEDGER].generation, 2);
    let cold = session(&host, Some(&checkpoint));
    for (step, outcome) in recorded.iter().chain(recorded.iter()) {
        publish(&cold, step, outcome.clone()).unwrap();
        assert_eq!(
            cold.export_state(),
            checkpoint,
            "{step} applies nothing again"
        );
    }
}

/// A refusal case: its step, its proposal and the refusal it must meet.
type RefusalCase = (&'static str, Proposal, fn(&StateCommandRefusal) -> bool);

/// One refused command publishes nothing of its batch: the refusal is one
/// recorded publication, typed, and no value changes.
#[tokio::test]
async fn one_refused_command_publishes_none_of_its_batch() {
    let reducer_calls = Arc::new(AtomicUsize::new(0));
    let host = host(&reducer_calls);
    let wide = || serde_json::json!("x".repeat(32766));
    let cases: Vec<RefusalCase> = vec![
        (
            "invalid-key",
            tool_commands(
                LEDGER,
                "invalid-key",
                StateCommands::new()
                    .set("kept", serde_json::json!(1))
                    .set("bad key", serde_json::json!(2)),
            ),
            |refusal| {
                matches!(
                    refusal,
                    StateCommandRefusal::InvalidKey {
                        index: 1,
                        reason: KeyRejection::IllegalCharacter { at: 3, byte: b' ' }
                    }
                )
            },
        ),
        (
            "too-many",
            tool_commands(
                LEDGER,
                "too-many",
                StateCommands::from(
                    (0..65)
                        .map(|index| StateCommand::Remove {
                            key: format!("k{index}"),
                        })
                        .collect::<Vec<_>>(),
                ),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::TooManyCommands { count: 65 }),
        ),
        (
            "wide-value",
            tool_commands(
                LEDGER,
                "wide-value",
                StateCommands::new()
                    .set("kept", serde_json::json!(1))
                    .set("wide", serde_json::json!("x".repeat(32767))),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::ValueTooLarge { index: 1, .. }),
        ),
        (
            "wide-namespace",
            tool_commands(
                LEDGER,
                "wide-namespace",
                StateCommands::new()
                    .set("a", wide())
                    .set("b", wide())
                    .set("c", wide())
                    .set("d", wide()),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::NamespaceTooLarge { .. }),
        ),
        (
            "unknown-reducer",
            tool_commands(
                LEDGER,
                "unknown-reducer",
                StateCommands::new()
                    .set("kept", serde_json::json!(1))
                    .apply("total", "missing", serde_json::json!(1)),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::UnknownReducer { index: 1, name } if name == "missing"),
        ),
        (
            "reducer-refusal",
            tool_commands(
                LEDGER,
                "reducer-refusal",
                StateCommands::new()
                    .set("kept", serde_json::json!(1))
                    .apply("total", "overdraw", serde_json::json!(1)),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::Reducer { index: 1, cause } if cause.error_type == "ledger.overdrawn"),
        ),
        (
            "reducer-panic",
            tool_commands(
                LEDGER,
                "reducer-panic",
                StateCommands::new().apply("total", "panics", serde_json::json!(1)),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::Reducer { index: 0, cause } if cause.error_type == "lash.state_reducer_panicked"),
        ),
        (
            "wrong-owner",
            Proposal::for_tool(
                lash_core_store::store::plugin_writers::PluginRevision::new(
                    LEDGER,
                    crate::plugin::BehaviorRevision::new(2).unwrap(),
                ),
                StateCommandOrigin::ToolAttempt {
                    call_id: crate::ToolCallId::fixture("wrong-owner"),
                    attempt: lash_core_store::tool_run::AttemptOrdinal::FIRST,
                },
                StateCommands::new().set("kept", serde_json::json!(1)),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::WrongOwner),
        ),
        (
            "decision-only",
            Proposal::for_callback(
                &lash_core_store::store::plugin_writers::PluginCallbackIdentity {
                    owner: revision(LEDGER),
                    key: "assistant_response:derive".into(),
                },
                StateCommandOrigin::ToolAttempt {
                    call_id: crate::ToolCallId::fixture("decision-only"),
                    attempt: lash_core_store::tool_run::AttemptOrdinal::FIRST,
                },
                StateCommands::new().set("kept", serde_json::json!(1)),
            ),
            |refusal| matches!(refusal, StateCommandRefusal::DecisionOnly),
        ),
    ];
    let live = session(&host, None);
    for (step, proposal, expected) in cases {
        let before = live.export_state().plugins[LEDGER].clone();
        let outcome = record(&live, step, vec![proposal]).await;
        let StateResolutionOutcome::Refused { refusal } = &resolutions(&outcome)[0].outcome else {
            panic!("{step}: the batch is refused: {outcome:?}");
        };
        assert!(expected(refusal), "{step}: {refusal:?}");
        let encoded = serde_json::to_value(refusal).unwrap();
        assert_eq!(
            &serde_json::from_value::<StateCommandRefusal>(encoded).unwrap(),
            refusal,
            "{step}: the refusal stays typed in the journal"
        );
        publish(&live, step, outcome).unwrap();
        let after = live.export_state().plugins[LEDGER].clone();
        assert_eq!(
            after.values, before.values,
            "{step}: nothing of the batch applies"
        );
        assert_eq!(
            after.generation,
            before.generation + 1,
            "{step}: one publication"
        );
    }

    live.state
        .lock_recover()
        .data
        .plugins
        .get_mut(LEDGER)
        .unwrap()
        .format_version = crate::FormatVersion::new(2).unwrap();
    let outcome = record(
        &live,
        "incompatible-writer",
        vec![tool_commands(
            LEDGER,
            "incompatible-writer",
            StateCommands::new().set("kept", serde_json::json!(1)),
        )],
    )
    .await;
    assert!(matches!(
        &resolutions(&outcome)[0].outcome,
        StateResolutionOutcome::Refused {
            refusal: StateCommandRefusal::IncompatibleWriter(crate::FormatRefusal {
                namespace: crate::FormatNamespace::State,
                ..
            })
        }
    ));
}

/// A durable resolution replays on a build whose reducer changed, without
/// running it.
#[tokio::test]
async fn a_recorded_resolution_replays_without_its_reducer() {
    let reducer_calls = Arc::new(AtomicUsize::new(0));
    let recording = host(&reducer_calls);
    let live = session(&recording, None);
    let base = live.export_state();
    let outcome = record(
        &live,
        "add",
        vec![tool_commands(
            LEDGER,
            "add",
            StateCommands::new().apply("total", "add", serde_json::json!(7)),
        )],
    )
    .await;
    publish(&live, "add", outcome.clone()).unwrap();
    assert_eq!(reducer_calls.load(Ordering::SeqCst), 1);

    let changed = crate::PluginHost::new(vec![
        Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial(LEDGER),
            PluginSpec::new().with_state_reducer(
                "add",
                Arc::new(|_: StateReduction<'_>| panic!("a recorded resolution never reduces")),
            ),
        )),
        Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial(OTHER),
            PluginSpec::new(),
        )),
    ]);
    let cold = session(&changed, Some(&base));
    publish(&cold, "add", journaled(&outcome)).unwrap();
    assert_eq!(value(&cold, LEDGER, "total"), Some(serde_json::json!(7)));
    assert_eq!(cold.export_state(), live.export_state());
}

/// A recorded failure publishes nothing, and leaves no reservation behind.
#[tokio::test]
async fn a_failed_body_publishes_nothing() {
    let host = host(&Arc::default());
    let live = session(&host, None);
    let base = live.export_state();
    let body_session = Arc::clone(&live);
    let failed = record_effect(
        Arc::clone(&live),
        RuntimeEffectKind::LanguageRuntimeValue,
        address("failed"),
        async move {
            propose(
                &body_session,
                tool_commands(
                    LEDGER,
                    "failed",
                    StateCommands::new().set("total", serde_json::json!(1)),
                ),
            )?;
            Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::Plugin,
                "the body failed",
            ))
        },
    )
    .await;
    assert!(failed.is_err());
    assert_eq!(live.export_state(), base);
    let next = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        record(
            &live,
            "next",
            vec![tool_commands(
                LEDGER,
                "next",
                StateCommands::new().set("total", serde_json::json!(2)),
            )],
        ),
    )
    .await
    .expect("a failure reserves nothing");
    publish(&live, "next", next).unwrap();
    assert_eq!(value(&live, LEDGER, "total"), Some(serde_json::json!(2)));
}

/// A reduced publication whose outcome the engine never returned may be
/// durable: its namespace publishes nothing more until it is rebuilt from
/// durable state, and the refusal is never the next body's recorded result.
#[tokio::test]
async fn an_abandoned_publication_fences_its_namespace_until_rebuilt() {
    let host = host(&Arc::default());
    let live = session(&host, None);
    let base = live.export_state();
    let publication = EffectPublication::begin(Arc::clone(&live), address("abandoned"));
    let _unreturned = record(
        &live,
        "abandoned",
        vec![tool_commands(
            LEDGER,
            "abandoned",
            StateCommands::new().set("total", serde_json::json!(1)),
        )],
    )
    .await;
    drop(publication);
    let body_session = Arc::clone(&live);
    let fenced = record_effect(
        Arc::clone(&live),
        RuntimeEffectKind::ToolAttempt,
        address("after"),
        async move {
            propose(
                &body_session,
                tool_commands(
                    LEDGER,
                    "after",
                    StateCommands::new().set("total", serde_json::json!(2)),
                ),
            )?;
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue { value: Value::Null })
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        fenced.cause,
        Some(crate::RuntimeErrorCause::PluginStatePublicationFenced {
            plugin: LEDGER.into()
        })
    );
    assert!(
        fenced
            .journal_disposition(RuntimeEffectKind::ToolAttempt)
            .is_retryable_derivation(),
        "the fence is never the body's recorded result"
    );
    let other = record(
        &live,
        "other",
        vec![tool_commands(
            OTHER,
            "other",
            StateCommands::new().set("x", serde_json::json!(1)),
        )],
    )
    .await;
    publish(&live, "other", other).unwrap();
    assert_eq!(value(&live, OTHER, "x"), Some(serde_json::json!(1)));

    live.hydrate_state(&base).unwrap();
    let after = record(
        &live,
        "rebuilt",
        vec![tool_commands(
            LEDGER,
            "rebuilt",
            StateCommands::new().set("total", serde_json::json!(3)),
        )],
    )
    .await;
    publish(&live, "rebuilt", after).unwrap();
    assert_eq!(value(&live, LEDGER, "total"), Some(serde_json::json!(3)));
}

/// After ownership moves to a later segment, a resolution an earlier segment
/// recorded and never applied is refused typed; one already applied is
/// delivered harmlessly.
#[tokio::test]
async fn a_predecessor_segment_never_publishes_after_transfer() {
    let host = host(&Arc::default());
    let predecessor = session(&host, None);
    let base = predecessor.export_state();
    let applied = record(
        &predecessor,
        "applied",
        vec![tool_commands(
            LEDGER,
            "applied",
            StateCommands::new().set("total", serde_json::json!(1)),
        )],
    )
    .await;
    publish(&predecessor, "applied", applied.clone()).unwrap();
    let checkpoint = predecessor.export_state();
    let stale = record(
        &predecessor,
        "stale",
        vec![tool_commands(
            LEDGER,
            "stale",
            StateCommands::new().set("total", serde_json::json!(2)),
        )],
    )
    .await;
    assert_ne!(base, checkpoint);

    let successor = session(&host, Some(&checkpoint));
    successor.adopt_state_segment(lash_core_store::tool_run::SegmentOrdinal(1));
    let transferred = successor.export_state();
    let successor = session(&host, Some(&transferred));
    publish(&successor, "applied", journaled(&applied)).unwrap();
    let refusal = publish(&successor, "stale", journaled(&stale)).unwrap_err();
    assert_eq!(
        refusal.cause,
        Some(crate::RuntimeErrorCause::PluginStateFrontier {
            refusal: Box::new(lash_core_store::tool_run::NamespaceFrontierRefusal {
                plugin: LEDGER.into(),
                refusal: lash_core_store::tool_run::FrontierRefusal::StalePublisher {
                    owner: 1,
                    found: 0,
                },
            }),
        })
    );
    assert_eq!(successor.export_state(), transferred);
}

/// L19 / FIG-4923: two recorded outcomes reduced from the same checkpoint
/// cannot share a publication ordinal and silently discard one outcome.
#[tokio::test]
async fn a_checkpoint_refuses_a_different_receipt_at_an_applied_ordinal() {
    let reducer_calls = Arc::new(AtomicUsize::new(0));
    let host = host(&reducer_calls);
    let predecessor = session(&host, None);
    let checkpoint = predecessor.export_state();
    let child = record(
        &predecessor,
        "child",
        vec![tool_commands(
            LEDGER,
            "child",
            StateCommands::new().set("k", serde_json::json!("a")),
        )],
    )
    .await;
    let successor = session(&host, Some(&checkpoint));
    let hook = record(
        &successor,
        "successor",
        vec![tool_commands(
            LEDGER,
            "successor",
            StateCommands::new().set("j", serde_json::json!("b")),
        )],
    )
    .await;
    assert_eq!(
        resolutions(&child)[0].ordinal,
        resolutions(&hook)[0].ordinal
    );
    publish(&successor, "successor", journaled(&hook)).unwrap();
    let accepted = successor.export_state();
    // Cross the checkpoint codec and reconstruct the publication coordinator.
    let restored: PluginState =
        rmp_serde::from_slice(&rmp_serde::to_vec_named(&accepted).unwrap()).unwrap();
    let cold = session(&host, Some(&restored));
    publish(&cold, "successor", journaled(&hook)).unwrap();
    let original = resolutions(&hook)[0].clone();
    let mut changed = vec![resolutions(&child)[0].clone()];
    let mut attempt = original.clone();
    attempt.origin = StateCommandOrigin::ToolAttempt {
        call_id: crate::ToolCallId::fixture("successor"),
        attempt: lash_core_store::tool_run::AttemptOrdinal::new(2).unwrap(),
    };
    changed.push(attempt);
    let mut phase = original.clone();
    phase.origin = StateCommandOrigin::DeferredFinalization {
        call_id: crate::ToolCallId::fixture("successor"),
        attempt: lash_core_store::tool_run::AttemptOrdinal::FIRST,
    };
    changed.push(phase);
    let mut run = original.clone();
    run.publisher.execution_scope = crate::ExecutionScope::turn("state-owner", "another-run");
    changed.push(run);
    let mut content = original.clone();
    content.outcome = StateResolutionOutcome::Applied { changes: vec![] };
    changed.push(content);
    let mut predecessor = original.clone();
    predecessor.predecessor = Some(PublicationOrdinal(9));
    changed.push(predecessor);
    for resolution in changed {
        let error = cold
            .publish_run_resolutions(&resolution.publisher.clone(), vec![resolution])
            .unwrap_err();
        assert_eq!(
            error.cause,
            Some(crate::RuntimeErrorCause::PluginStateFrontier {
                refusal: Box::new(NamespaceFrontierRefusal {
                    plugin: LEDGER.into(),
                    refusal: FrontierRefusal::ReceiptMismatch { found: 1 },
                }),
            })
        );
        assert_eq!(cold.export_state(), accepted);
    }
    assert_eq!(cold.export_state(), accepted);
    assert_eq!(value(&cold, LEDGER, "j"), Some(serde_json::json!("b")));
    assert_eq!(value(&cold, LEDGER, "k"), None);
    assert_eq!(reducer_calls.load(Ordering::SeqCst), 0);
}

/// L19: a callback retains its starting segment across an await. Its recorded
/// outcome cannot publish after a successor takes ownership.
#[tokio::test]
async fn a_callback_keeps_its_publisher_segment_across_handover() {
    let host = host(&Arc::default());
    let live = session(&host, None);
    let body_session = Arc::clone(&live);
    let (began, reached) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let body = record_effect(
        Arc::clone(&live),
        RuntimeEffectKind::LanguageRuntimeValue,
        address("held"),
        async move {
            began.send(()).unwrap();
            finished.await.unwrap();
            propose(
                &body_session,
                tool_commands(
                    LEDGER,
                    "held",
                    StateCommands::new().set("k", serde_json::json!("a")),
                ),
            )?;
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue { value: Value::Null })
        },
    );
    tokio::pin!(body);
    tokio::select! {
        result = &mut body => panic!("held callback finished: {result:?}"),
        _ = reached => {}
    }
    live.adopt_state_segment(SegmentOrdinal(1));
    let checkpoint = live.export_state();
    finish.send(()).unwrap();
    let outcome = journaled(&body.await.unwrap());
    assert_eq!(resolutions(&outcome)[0].segment, SegmentOrdinal(0));
    assert_eq!(live.export_state(), checkpoint, "reduction remains private");
    let error = publish(&live, "held", outcome).unwrap_err();
    assert_eq!(
        error.cause,
        Some(crate::RuntimeErrorCause::PluginStateFrontier {
            refusal: Box::new(NamespaceFrontierRefusal {
                plugin: LEDGER.into(),
                refusal: FrontierRefusal::StalePublisher { owner: 1, found: 0 },
            }),
        })
    );
    assert_eq!(live.export_state(), checkpoint);
}
