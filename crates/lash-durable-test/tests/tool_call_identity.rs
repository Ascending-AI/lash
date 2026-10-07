//! The tool-call identity laws through a host's `send()` on a served node
//! (FIG-4079, FIG-4073, ADR 0117; ported by FIG-5210 from the deleted
//! lash-conformance `tool_call_identity` suite).
//!
//! A tool author keys idempotency on the identity lash hands its attempt:
//! the `ToolCallId` an attempt reads from `AttemptContext::call_id()`,
//! beside its attempt number. It must stay the same across every re-run of
//! one logical call (a retry after a reported failure, a resume of a turn a
//! node lost) and differ for every other call, even when the model's
//! provider hands two calls the same call id. The kill-and-resume half is
//! `tool_crash_laws.rs`; a call a process body issues is FIG-5216's.
//!
//! Every law runs real turns: a host creates sessions on a core over the
//! tier's database and sends them inputs, and the core's own node runs each
//! turn on the durable path with the law's scripted model and probe tools.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::{ToolCall, ToolOutcome};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, World};

/// The probe that answers at once, holds, or fails its first attempt.
const PROBE: &str = "identity_probe";
/// The probe that parks on its completion key and resolves it itself.
const DEFERRED: &str = "identity_deferred";

/// One execution of a probe's body.
#[derive(Clone, Debug)]
struct Execution {
    /// The call's `label` argument: which logical call the law meant.
    label: String,
    call_id: lash_core::ToolCallId,
    attempt: u32,
    /// The completion key a deferred probe parked on.
    completion_key: Option<String>,
}

/// Everything the probes did, and the gate a held probe waits on.
struct Witness {
    executions: Mutex<Vec<Execution>>,
    gate: tokio::sync::watch::Sender<bool>,
}

impl Default for Witness {
    fn default() -> Self {
        Self {
            executions: Mutex::default(),
            gate: tokio::sync::watch::channel(false).0,
        }
    }
}

impl Witness {
    fn of(&self, label: &str) -> Vec<Execution> {
        self.executions
            .lock_recover()
            .iter()
            .filter(|execution| execution.label == label)
            .cloned()
            .collect()
    }

    /// The one execution of `label`'s body.
    fn only(&self, label: &str) -> Execution {
        let executions = self.of(label);
        assert_eq!(
            executions.len(),
            1,
            "`{label}`'s body ran exactly once: {executions:?}"
        );
        executions[0].clone()
    }

    fn open(&self) {
        self.gate.send_replace(true);
    }

    async fn passed(&self) {
        let mut gate = self.gate.subscribe();
        gate.wait_for(|open| *open)
            .await
            .expect("the witness outlives its probes");
    }
}

fn probe_definition(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    let definition = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Records the identity its attempt saw and answers with its label.",
        object.clone(),
        object,
    )
    .expect("the probe's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ));
    if name == DEFERRED {
        definition.with_declaration(lash_core::ToolDeclaration::deferring())
    } else {
        definition
    }
}

struct Probes {
    witness: Arc<Witness>,
    backend: lash::Backend,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Probes {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        [PROBE, DEFERRED]
            .into_iter()
            .map(|name| probe_definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        [PROBE, DEFERRED]
            .contains(&name)
            .then(|| Arc::new(probe_definition(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        let hold = call.args["hold"].as_bool().unwrap_or(false);
        let fail_first = call.args["fail_first"].as_bool().unwrap_or(false);
        let attempt = call.context.attempt_number();
        if call.name() == DEFERRED {
            let key = call
                .context
                .completion_key()
                .expect("a deferring round member's completion wait is pinned");
            self.witness.executions.lock_recover().push(Execution {
                label: label.clone(),
                call_id: call.context.call_id().clone(),
                attempt,
                completion_key: Some(key.as_str().to_owned()),
            });
            // The call resolves its own key with its own label: a call that
            // reads another label consumed another call's completion.
            let witness = Arc::clone(&self.witness);
            let backend = self.backend.clone();
            tokio::spawn(async move {
                if hold {
                    witness.passed().await;
                }
                lash_core::waits::resolve_host(
                    &backend,
                    key.as_str(),
                    lash_core::Resolution::Ok(serde_json::json!({ "label": label })),
                )
                .await
                .expect("the deferred probe's wait resolves");
            });
            return lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new());
        }
        self.witness.executions.lock_recover().push(Execution {
            label: label.clone(),
            call_id: call.context.call_id().clone(),
            attempt,
            completion_key: None,
        });
        if hold {
            self.witness.passed().await;
        }
        if fail_first && attempt == 1 {
            return ToolOutcome::failure(lash_core::ToolFailure::with_suggested_delay(
                lash_core::ToolFailureClass::External,
                "identity_probe_timeout",
                "the probe's effect happened and its first attempt reported a timeout",
                Some(1),
            ))
            .into();
        }
        ToolOutcome::ok(serde_json::json!({ "label": label })).into()
    }
}

/// A core running the standard protocol (or the RLM protocol, for `code`)
/// with the probes.
async fn world(tier: Tier, code: bool) -> Option<(World, Arc<Witness>)> {
    let witness = Arc::new(Witness::default());
    let probes = Arc::clone(&witness);
    let world = World::new(tier, move |backend| {
        let builder = if code {
            lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, None))
        } else {
            lash::LashCore::standard_builder(backend.clone())
        };
        builder.tools(Arc::new(Probes {
            witness: probes,
            backend: backend.clone(),
        }))
    })
    .await?;
    Some((world, witness))
}

fn probe(call_id: &str, tool: &str, args: serde_json::Value) -> lash_core::llm::types::LlmResponse {
    served::response(vec![served::call(call_id, tool, args)])
}

/// The labels the transcript's tool results answered, in order.
fn answered_labels(output: &lash::TurnOutput) -> Vec<String> {
    served::results(output)
        .iter()
        .map(|answered| {
            answered.value()["label"]
                .as_str()
                .unwrap_or("<not a label>")
                .to_owned()
        })
        .collect()
}

/// Two turns of one session whose provider emits the same call id,
/// `call_0`, are two logical calls: the identity a tool keys idempotency on
/// differs between them, and each call answers for itself.
async fn repeated_provider_id_across_turns_is_distinct(tier: Tier) {
    let Some((world, witness)) = world(tier, false).await else {
        return;
    };
    let session = world
        .session("repeated-provider-id", served::spec(64))
        .await;
    let mut answered = Vec::new();
    for name in ["repeated-first", "repeated-second"] {
        world.script(
            name,
            vec![probe("call_0", PROBE, serde_json::json!({ "label": name }))],
        );
        let output = world.send(&session, name).await;
        served::assert_answered(name, &output);
        answered.push(name.to_owned());
        assert_eq!(
            answered_labels(&output),
            answered,
            "{name}: each turn's call answered for itself"
        );
    }
    let first = witness.only("repeated-first");
    let second = witness.only("repeated-second");
    assert_ne!(
        first.call_id, second.call_id,
        "two turns whose provider emitted `call_0` each are two calls"
    );
    world.shutdown().await;
}

/// Two deferred calls in one turn, one model step each, whose provider gave
/// both the id `call_0`, never consume each other's completion: each parks
/// on its own key and settles with its own resolution.
async fn same_scope_completion_collision(tier: Tier) {
    let Some((world, witness)) = world(tier, false).await else {
        return;
    };
    let name = "same-scope-completion";
    let output = world
        .run(
            name,
            served::spec(64),
            vec![
                probe("call_0", DEFERRED, serde_json::json!({ "label": "first" })),
                probe("call_0", DEFERRED, serde_json::json!({ "label": "second" })),
            ],
        )
        .await;
    served::assert_answered(name, &output);
    assert_eq!(
        answered_labels(&output),
        vec!["first", "second"],
        "each deferred call settles with its own resolution"
    );
    let first = witness.only("first");
    let second = witness.only("second");
    assert_ne!(
        first.completion_key, second.completion_key,
        "two calls in one turn park on distinct completion keys"
    );
    assert_ne!(
        first.call_id, second.call_id,
        "two calls in one turn whose provider emitted `call_0` each are two calls"
    );
    world.shutdown().await;
}

/// A reported failure after the effect (a timeout, say) is retried under
/// the same call id: the first attempt's effect may have happened, so a key
/// that changed would defeat the tool's deduplication. The attempt number
/// advances, and the retry's success is the call's outcome.
async fn reported_failure_retry_preserves_call_id(tier: Tier) {
    let Some((world, witness)) = world(tier, false).await else {
        return;
    };
    let name = "reported-failure-retry";
    let output = world
        .run(
            name,
            served::spec(64),
            vec![probe(
                "call_retry",
                PROBE,
                serde_json::json!({ "label": "retried", "fail_first": true }),
            )],
        )
        .await;
    served::assert_answered(name, &output);
    let executions = witness.of("retried");
    assert_eq!(
        executions
            .iter()
            .map(|execution| execution.attempt)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the reported failure is retried once, and the attempt number advances"
    );
    assert!(
        executions
            .iter()
            .all(|execution| execution.call_id == executions[0].call_id),
        "every attempt of one call saw one call id: {executions:?}"
    );
    assert_eq!(
        answered_labels(&output),
        vec!["retried"],
        "the retry's success is the call's outcome"
    );
    world.shutdown().await;
}

/// The session's head, read from its store.
async fn head(world: &World, session: &str) -> lash_core::store::SessionHeadMeta {
    let factory = world.backend.session_store_factory();
    lash_core::SessionCommitStore::load_session_head_meta(
        factory.as_ref(),
        &lash::SessionId::try_from(session.to_owned()).expect("a session id"),
    )
    .await
    .expect("the head reads")
    .expect("the session has a head")
}

/// The settled session's history nodes, by id.
fn nodes(output: &lash::TurnOutput) -> BTreeMap<String, serde_json::Value> {
    let graph =
        serde_json::to_value(&output.result.state.session_graph).expect("the graph encodes");
    graph["nodes"]
        .as_array()
        .expect("the graph's nodes")
        .iter()
        .map(|node| {
            (
                node["node_id"].as_str().unwrap_or_default().to_owned(),
                node.clone(),
            )
        })
        .collect()
}

/// A turn of two tool steps commits the session's history once, and never
/// replaces a node an earlier turn committed: the earlier nodes are kept
/// byte for byte and the turn's own are appended after them.
async fn one_turn_commits_history_once_and_never_replaces_existing_nodes(tier: Tier) {
    let Some((world, witness)) = world(tier, false).await else {
        return;
    };
    let session_name = "single-history-commit";
    let session = world.session(session_name, served::spec(64)).await;
    world.script("history-seed", Vec::new());
    let seed = world.send(&session, "history-seed").await;
    served::assert_answered("seed", &seed);
    let before = head(&world, session_name).await;
    let prefix = nodes(&seed);

    world.script(
        "history-progress",
        vec![
            probe(
                "call_0",
                PROBE,
                serde_json::json!({ "label": "first-progress" }),
            ),
            probe(
                "call_1",
                PROBE,
                serde_json::json!({ "label": "second-progress" }),
            ),
        ],
    );
    let progress = world.send(&session, "history-progress").await;
    served::assert_answered("progress", &progress);
    witness.only("first-progress");
    witness.only("second-progress");
    let after = head(&world, session_name).await;
    assert_eq!(
        after.head_revision,
        before.head_revision + 1,
        "the turn commits the session's history once"
    );
    let grown = nodes(&progress);
    for (id, node) in &prefix {
        assert_eq!(
            grown.get(id),
            Some(node),
            "the turn never replaces node {id} an earlier turn committed"
        );
    }
    assert!(grown.len() > prefix.len(), "the turn appends its own nodes");
    world.shutdown().await;
}

/// While a tool's call is parked on its completion, the turn holds the
/// session's head: no history node is appended and the head's revision,
/// leaf, frame, config and follow-on are as before the turn. The host's
/// resolution settles the call, and the turn then commits once.
async fn suspended_tool_keeps_turn_and_history_head_until_resolution(tier: Tier) {
    let Some((world, witness)) = world(tier, false).await else {
        return;
    };
    let session_name = "suspended-history";
    let session = world.session(session_name, served::spec(64)).await;
    world.script("suspended-seed", Vec::new());
    served::assert_answered("seed", &world.send(&session, "suspended-seed").await);
    let before = head(&world, session_name).await;
    world.script(
        "suspended-held",
        vec![probe(
            "held",
            DEFERRED,
            serde_json::json!({ "label": "suspended", "hold": true }),
        )],
    );
    let (output, ()) = tokio::join!(world.send(&session, "suspended-held"), async {
        while witness.of("suspended").is_empty() {
            tokio::task::yield_now().await;
        }
        let parked = witness.only("suspended");
        let key = parked.completion_key.expect("the parked call's key");
        let outstanding = world
            .core
            .completions()
            .outstanding(session.session_id())
            .await
            .expect("the session's outstanding completions read");
        assert!(
            outstanding.iter().any(|pinned| pinned.as_str() == key),
            "the parked call's wait is outstanding while it waits"
        );
        let waiting = head(&world, session_name).await;
        assert_eq!(
            waiting.head_revision, before.head_revision,
            "the turn commits nothing while its tool waits"
        );
        assert_eq!(
            waiting.leaf_node_id, before.leaf_node_id,
            "no history node is appended while the tool waits"
        );
        assert_eq!(waiting.current_frame_node_id, before.current_frame_node_id);
        assert_eq!(waiting.config, before.config);
        witness.open();
    });
    served::assert_answered("resolved suspended turn", &output);
    let after = head(&world, session_name).await;
    assert_eq!(
        after.head_revision,
        before.head_revision + 1,
        "the resolved turn commits once"
    );
    witness.only("suspended");
    assert_eq!(answered_labels(&output), vec!["suspended"]);
    world.shutdown().await;
}

/// A fork inherits its source's history and nothing of its execution: no
/// pending input, no outstanding wait, no recorded outcome. The same call
/// at the same position of a turn on the fork runs its body again, under
/// an identity of its own, and the source's history is untouched by it.
async fn fork_inherits_history_without_execution_queues_waits_or_journals(tier: Tier) {
    let Some((world, witness)) = world(tier, false).await else {
        return;
    };
    let source_name = "fork-source";
    let source = world.session(source_name, served::spec(64)).await;
    world.script(
        "fork-source-turn",
        vec![probe(
            "call_0",
            PROBE,
            serde_json::json!({ "label": "same" }),
        )],
    );
    let seeded = world.send(&source, "fork-source-turn").await;
    served::assert_answered("source", &seeded);
    let revision = head(&world, source_name).await.head_revision;

    let branch_id = lash::SessionId::try_from("fork-branch".to_owned()).expect("a session id");
    world
        .core
        .fork_at(
            source.session_id(),
            lash_core::Target::Revision(revision),
            lash::ForkRequest {
                session_id: branch_id.clone(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: source.session_id().clone(),
                    source_node_id: None,
                },
                observed_processes: Vec::new(),
            },
        )
        .await
        .expect("the fork is taken after the source's turn ended");
    let branch = world
        .core
        .session(branch_id)
        .durable()
        .await
        .expect("the fork opens");
    assert!(
        branch
            .pending_turn_inputs()
            .await
            .expect("the fork's inputs read")
            .is_empty(),
        "the fork copies no pending input"
    );
    assert!(
        world
            .core
            .completions()
            .outstanding(branch.session_id())
            .await
            .expect("the fork's waits read")
            .is_empty(),
        "the fork copies no wait"
    );

    world.script(
        "fork-branch-turn",
        vec![probe(
            "call_0",
            PROBE,
            serde_json::json!({ "label": "same" }),
        )],
    );
    let forked = world.send(&branch, "fork-branch-turn").await;
    served::assert_answered("branch", &forked);
    let executions = witness.of("same");
    assert_eq!(
        executions.len(),
        2,
        "the fork's call runs its own body; it reads no outcome of its source: {executions:?}"
    );
    assert_ne!(
        executions[0].call_id, executions[1].call_id,
        "the same call at the same position of a fork is another call"
    );
    assert_eq!(
        answered_labels(&forked),
        vec!["same", "same"],
        "the fork's transcript holds its source's call and its own"
    );
    assert_eq!(
        head(&world, source_name).await.head_revision,
        revision,
        "a turn on the fork commits nothing to its source"
    );
    world.shutdown().await;
}

/// An RLM turn of two cells, the first calling the probe once and the
/// second twice: every call ran once, and the three calls are three
/// identities, though each is the same tool called from the same kind of
/// statement.
async fn code_cells_keep_identity_and_distinguish_fresh_calls(tier: Tier) {
    let Some((world, witness)) = world(tier, true).await else {
        return;
    };
    let name = "code-cells";
    let output = world
        .run(
            name,
            served::spec(64),
            vec![
                served::cell(&format!(r#"await tools.{PROBE}({{ label: "cell-one" }});"#)),
                served::cell(&format!(
                    "await tools.{PROBE}({{ label: \"cell-two-a\" }});\nfinish(await tools.{PROBE}({{ label: \"cell-two-b\" }}));"
                )),
            ],
        )
        .await;
    served::assert_answered(name, &output);
    let one = witness.only("cell-one");
    let two_a = witness.only("cell-two-a");
    let two_b = witness.only("cell-two-b");
    for (what, left, right) in [
        ("two cells' calls", &one, &two_a),
        ("two calls of one cell", &two_a, &two_b),
        ("the first cell's call and the last", &one, &two_b),
    ] {
        assert_ne!(left.call_id, right.call_id, "{what} are two identities");
    }
    world.shutdown().await;
}

tiered_laws!(
    repeated_provider_id_across_turns_is_distinct,
    same_scope_completion_collision,
    reported_failure_retry_preserves_call_id,
    one_turn_commits_history_once_and_never_replaces_existing_nodes,
    suspended_tool_keeps_turn_and_history_head_until_resolution,
    fork_inherits_history_without_execution_queues_waits_or_journals,
    code_cells_keep_identity_and_distinguish_fresh_calls,
);
